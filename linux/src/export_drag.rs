use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use url::Url;

pub const DRAG_MARKER: &str = ".clipboardline-drag.json";
pub const DRAG_OWNER: &str = "clipboardline-native-drag-v1";

fn reserved_device_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(?:con|prn|aux|nul|com[1-9¹²³]|lpt[1-9¹²³])(\.|$)").unwrap())
}

fn batch_dir_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^batch-[A-Za-z0-9]{6}$").unwrap())
}

fn drag_filename_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}-[-0-9a-f]{36}\.(txt|url|desktop)$").unwrap())
}

fn uuid_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[-0-9a-f]{36}$").unwrap())
}

pub fn decode_text_file(contents: &[u8]) -> Result<String, String> {
    if contents.len() >= 2 && contents[0] == 0xff && contents[1] == 0xfe {
        let utf16le = &contents[2..];
        if utf16le.len() % 2 != 0 {
            return Err("Invalid UTF-16LE text file.".to_string());
        }
        let units: Vec<u16> = utf16le
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units)
            .map_err(|_| "Invalid UTF-16LE text file.".to_string());
    }
    if contents.len() >= 2 && contents[0] == 0xfe && contents[1] == 0xff {
        let utf16be = &contents[2..];
        if utf16be.len() % 2 != 0 {
            return Err("Invalid UTF-16BE text file.".to_string());
        }
        let units: Vec<u16> = utf16be
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units)
            .map_err(|_| "Invalid UTF-16BE text file.".to_string());
    }
    let utf8 = if contents.len() >= 3
        && contents[0] == 0xef
        && contents[1] == 0xbb
        && contents[2] == 0xbf
    {
        &contents[3..]
    } else {
        contents
    };
    std::str::from_utf8(utf8)
        .map(|s| s.to_string())
        .map_err(|_| "Invalid UTF-8 text file.".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkFileEntry {
    pub extension: String,
    pub bytes: String,
}

pub fn link_file(url: &str, platform: &str) -> Result<LinkFileEntry, String> {
    if url.is_empty()
        || url
            .chars()
            .any(|c| (c as u32) <= 0x1f || c == '\x7f')
    {
        return Err("Invalid link.".to_string());
    }
    let parsed = Url::parse(url).map_err(|_| "Invalid link.".to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Invalid link.".to_string());
    }
    if platform == "linux" {
        let raw_host = parsed.host_str().filter(|s| !s.is_empty()).unwrap_or("Link");
        let name: String = raw_host
            .chars()
            .filter(|&c| (c as u32) > 0x1f && c != '\x7f')
            .collect();
        let safe_name = if name.is_empty() { "Link" } else { &name };
        let escaped_url = url.replace('\\', "\\\\");
        Ok(LinkFileEntry {
            extension: ".desktop".to_string(),
            bytes: format!(
                "[Desktop Entry]\nType=Link\nName={}\nURL={}\nIcon=text-html\n",
                safe_name, escaped_url
            ),
        })
    } else {
        Ok(LinkFileEntry {
            extension: ".url".to_string(),
            bytes: format!("[InternetShortcut]\r\nURL={}\r\n", url),
        })
    }
}

pub fn sanitize_export_label(raw: &str, fallback_kind: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| {
            if ['<', '>', ':', '"', '/', '\\', '|', '?', '*'].contains(&c) || (c as u32) <= 0x1f {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']);
    let mut utf16_units = 0_usize;
    let mut sliced = String::new();
    for ch in trimmed.chars() {
        let len16 = ch.len_utf16();
        if utf16_units + len16 > 100 {
            break;
        }
        utf16_units += len16;
        sliced.push(ch);
    }
    let final_label = if reserved_device_re().is_match(&sliced) {
        format!("_{}", sliced)
    } else {
        sliced
    };
    if final_label.is_empty() {
        fallback_kind.to_string()
    } else {
        final_label
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportResult {
    pub count: usize,
    pub failed: usize,
    pub message: String,
}

pub fn export_copies<F>(
    selected: &[Value],
    directory: &Path,
    platform: &str,
    media_path: F,
) -> Result<ExportResult, String>
where
    F: Fn(&str) -> Result<PathBuf, String>,
{
    let mut exported = 0_usize;
    let mut failed = 0_usize;

    for item in selected {
        let step = (|| -> Result<(), String> {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Invalid item".to_string())?;
            let kind = item
                .get("kind")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Invalid kind".to_string())?;
            let raw_label = item
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    item.get("title")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or(kind);
            let label = sanitize_export_label(raw_label, kind);

            let link = if kind == "link" {
                let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
                Some(link_file(url, platform)?)
            } else {
                None
            };

            let is_rec = item
                .get("recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let extension = if kind == "image" {
                if is_rec {
                    let mp = media_path(id)?;
                    mp.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| format!(".{}", e))
                        .unwrap_or_else(|| ".mp4".to_string())
                } else {
                    ".png".to_string()
                }
            } else if let Some(ref l) = link {
                l.extension.clone()
            } else {
                ".txt".to_string()
            };

            let mut index = 1_usize;
            let destination = loop {
                let candidate_name = if index > 1 {
                    format!("{}-{}{}", label, index, extension)
                } else {
                    format!("{}{}", label, extension)
                };
                let candidate = directory.join(candidate_name);
                if !candidate.exists() {
                    break candidate;
                }
                index += 1;
            };

            if kind == "image" {
                let src = media_path(id)?;
                let mut src_file = fs::File::open(&src).map_err(|e| e.to_string())?;
                let mut dst_file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| e.to_string())?;
                std::io::copy(&mut src_file, &mut dst_file).map_err(|e| e.to_string())?;
            } else {
                let bytes = if let Some(ref l) = link {
                    l.bytes.as_bytes()
                } else {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .as_bytes()
                };
                let mut dst_file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| e.to_string())?;
                dst_file.write_all(bytes).map_err(|e| e.to_string())?;
            }
            Ok(())
        })();

        match step {
            Ok(()) => exported += 1,
            Err(_) => failed += 1,
        }
    }

    if exported == 0 {
        return Err("Could not export copies".to_string());
    }
    Ok(ExportResult {
        count: exported,
        failed,
        message: format!("Exported {} · failed {}", exported, failed),
    })
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

pub struct DragFiles {
    pub directory: PathBuf,
    pub batches: HashSet<PathBuf>,
}

impl DragFiles {
    pub fn new(base_directory: impl AsRef<Path>) -> Result<Self, String> {
        let directory = base_directory.as_ref().join("native-drag");
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700));
        let meta = fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            return Err("Unsafe drag staging directory".to_string());
        }
        let mut drag = Self {
            directory: directory.clone(),
            batches: HashSet::new(),
        };
        if let Ok(entries) = fs::read_dir(&directory) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if batch_dir_re().is_match(name) {
                        drag.remove(&directory.join(name));
                    }
                }
            }
        }
        Ok(drag)
    }

    fn create_batch_dir(&self) -> Result<PathBuf, String> {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        for _ in 0..64 {
            let mut suffix = String::with_capacity(6);
            for _ in 0..6 {
                let idx = (rand::random::<u8>() as usize) % CHARS.len();
                suffix.push(CHARS[idx] as char);
            }
            let candidate = self.directory.join(format!("batch-{}", suffix));
            if fs::create_dir(&candidate).is_ok() {
                let _ = fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700));
                return Ok(candidate);
            }
        }
        Err("Could not allocate drag staging directory.".to_string())
    }

    pub fn prepare<F>(
        &mut self,
        selected: &[Value],
        platform: &str,
        media_path: F,
    ) -> Result<(Vec<PathBuf>, Option<PathBuf>), String>
    where
        F: Fn(&str) -> Result<PathBuf, String>,
    {
        if selected.is_empty() || selected.len() > 1000 {
            return Err("Select up to 1000 items to drag.".to_string());
        }
        struct DragRecord {
            id: String,
            kind: String,
            text: String,
            url: String,
        }
        let mut records = Vec::with_capacity(selected.len());
        for val in selected {
            let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let kind = val.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let trashed = val.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
            if !uuid_id_re().is_match(id)
                || !["image", "text", "link"].contains(&kind)
                || trashed
            {
                return Err("A dragged item is no longer available.".to_string());
            }
            let text = val.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let url = val.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if kind != "image" {
                let payload_field = if kind == "link" {
                    val.get("url")
                } else {
                    val.get("text")
                };
                let Some(s) = payload_field.and_then(|v| v.as_str()) else {
                    return Err("Invalid drag content.".to_string());
                };
                if s.len() > 1024 * 1024 {
                    return Err("Invalid drag content.".to_string());
                }
            }
            if kind == "link" {
                let Ok(parsed) = Url::parse(url) else {
                    return Err("Invalid dragged link.".to_string());
                };
                if (parsed.scheme() != "http" && parsed.scheme() != "https")
                    || url.contains(['\r', '\n', '\0'])
                {
                    return Err("Invalid dragged link.".to_string());
                }
            }
            records.push(DragRecord {
                id: id.to_string(),
                kind: kind.to_string(),
                text: text.to_string(),
                url: url.to_string(),
            });
        }

        let mut batch_dir: Option<PathBuf> = None;
        let mut generated: Vec<String> = Vec::new();

        let res = (|| -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
            if records.iter().any(|r| r.kind != "image") {
                let bdir = self.create_batch_dir()?;
                let mut names = Vec::new();
                for (index, rec) in records.iter().enumerate() {
                    if rec.kind != "image" {
                        let ext = if rec.kind == "link" {
                            link_file(&rec.url, platform)?.extension
                        } else {
                            ".txt".to_string()
                        };
                        names.push(format!("{:04}-{}{}", index + 1, rec.id, ext));
                    }
                }
                let marker_path = bdir.join(DRAG_MARKER);
                let mut mf = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&marker_path)
                    .map_err(|e| e.to_string())?;
                mf.write_all(
                    json!({ "owner": DRAG_OWNER, "files": names })
                        .to_string()
                        .as_bytes(),
                )
                .map_err(|e| e.to_string())?;
                batch_dir = Some(bdir);
            }

            let mut files = Vec::with_capacity(records.len());
            for (index, rec) in records.iter().enumerate() {
                if rec.kind == "image" {
                    let file = media_path(&rec.id)?;
                    if !is_regular_file(&file) {
                        return Err("Managed image is unavailable.".to_string());
                    }
                    files.push(file);
                } else {
                    let bdir = batch_dir.as_ref().unwrap();
                    let link = if rec.kind == "link" {
                        Some(link_file(&rec.url, platform)?)
                    } else {
                        None
                    };
                    let ext = link
                        .as_ref()
                        .map(|l| l.extension.as_str())
                        .unwrap_or(".txt");
                    let name = format!("{:04}-{}{}", index + 1, rec.id, ext);
                    let file = bdir.join(&name);
                    let bytes = link
                        .as_ref()
                        .map(|l| l.bytes.as_bytes())
                        .unwrap_or_else(|| rec.text.as_bytes());
                    let mut f = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&file)
                        .map_err(|e| e.to_string())?;
                    f.write_all(bytes).map_err(|e| e.to_string())?;
                    generated.push(name);
                    files.push(file);
                }
            }
            if let Some(ref bdir) = batch_dir {
                self.batches.insert(bdir.clone());
            }
            Ok((files, batch_dir.clone()))
        })();

        if res.is_err() {
            if let Some(bdir) = batch_dir {
                for name in generated {
                    let _ = fs::remove_file(bdir.join(name));
                }
                let _ = fs::remove_file(bdir.join(DRAG_MARKER));
                let _ = fs::remove_dir(&bdir);
            }
        }
        res
    }

    pub fn remove(&mut self, batch: &Path) -> bool {
        let Some(parent) = batch.parent() else {
            return false;
        };
        let Some(base) = batch.file_name().and_then(|s| s.to_str()) else {
            return false;
        };
        if parent != self.directory || !batch_dir_re().is_match(base) {
            return false;
        }
        let Ok(meta) = fs::symlink_metadata(batch) else {
            return false;
        };
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            return false;
        }
        let marker = batch.join(DRAG_MARKER);
        if !is_regular_file(&marker) {
            return false;
        }
        let Ok(marker_meta) = fs::metadata(&marker) else {
            return false;
        };
        if marker_meta.len() > 100_000 {
            return false;
        }
        let Ok(raw) = fs::read_to_string(&marker) else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        if value.get("owner").and_then(|v| v.as_str()) != Some(DRAG_OWNER) {
            return false;
        }
        let Some(files_arr) = value.get("files").and_then(|v| v.as_array()) else {
            return false;
        };
        if files_arr.len() > 1000 {
            return false;
        }
        let mut expected_files = HashSet::new();
        for f_val in files_arr {
            let Some(name) = f_val.as_str() else {
                return false;
            };
            if !drag_filename_re().is_match(name) || !expected_files.insert(name.to_string()) {
                return false;
            }
        }
        let mut expected = expected_files.clone();
        expected.insert(DRAG_MARKER.to_string());

        let Ok(entries) = fs::read_dir(batch) else {
            return false;
        };
        let mut actual = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                return false;
            };
            if !expected.contains(&name) || !is_regular_file(&batch.join(&name)) {
                return false;
            }
            actual.push(name);
        }
        if actual.len() > expected.len() {
            return false;
        }
        for name in &expected_files {
            if actual.contains(name) {
                let _ = fs::remove_file(batch.join(name));
            }
        }
        let _ = fs::remove_file(&marker);
        if fs::remove_dir(batch).is_ok() {
            self.batches.remove(batch);
            true
        } else {
            false
        }
    }

    pub fn dispose(&mut self) {
        let current: Vec<PathBuf> = self.batches.iter().cloned().collect();
        for b in current {
            self.remove(&b);
        }
    }
}
