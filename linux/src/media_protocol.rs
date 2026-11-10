use regex::Regex;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

pub const LOCAL_ORIGINS: [&str; 4] = [
    "null",
    "file://",
    "tauri://localhost",
    "http://tauri.localhost",
];

fn requested_gen_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\?v=([a-f0-9]{64})$").unwrap())
}

fn managed_file_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([a-f0-9]{64})\.(png|mp4|mov)$").unwrap())
}

fn byte_range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^bytes=(\d*)-(\d*)$").unwrap())
}

pub fn media_origin(
    method: &str,
    origin: Option<&str>,
    initiator: Option<&str>,
) -> Option<String> {
    if method != "GET" {
        return None;
    }
    let orig = origin.filter(|s| !s.is_empty());
    let init = initiator.filter(|s| !s.is_empty());
    if let Some(o) = orig {
        if !LOCAL_ORIGINS.contains(&o) {
            return None;
        }
    }
    if let Some(i) = init {
        if !LOCAL_ORIGINS.contains(&i) {
            return None;
        }
    }
    if let (Some(o), Some(i)) = (orig, init) {
        if o != i {
            return None;
        }
    }
    Some(orig.unwrap_or("null").to_string())
}

pub fn media_generation_matches(search: Option<&str>, route: Option<&str>, file: &Path) -> bool {
    let Some(s) = search.filter(|q| !q.is_empty()) else {
        return true;
    };
    let with_q = if s.starts_with('?') {
        s.to_string()
    } else {
        format!("?{}", s)
    };
    let Some(req_caps) = requested_gen_re().captures(&with_q) else {
        return false;
    };
    let Some(base) = file.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(man_caps) = managed_file_re().captures(base) else {
        return false;
    };
    if req_caps.get(1).map(|m| m.as_str()) != man_caps.get(1).map(|m| m.as_str()) {
        return false;
    }
    let ext = man_caps.get(2).map(|m| m.as_str()).unwrap_or("");
    if route == Some("/video") {
        ext == "mp4" || ext == "mov"
    } else {
        (route.is_none() || route == Some("") || route == Some("/thumbnail")) && ext == "png"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
    pub partial: bool,
}

pub fn byte_range(value: Option<&str>, size: u64) -> Option<ByteRange> {
    if size == 0 || size > 9_007_199_254_740_991 {
        return None;
    }
    let Some(val) = value else {
        return Some(ByteRange {
            start: 0,
            end: size - 1,
            partial: false,
        });
    };
    if val.len() > 100 {
        return None;
    }
    let caps = byte_range_re().captures(val)?;
    let first_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let last_str = caps.get(2).map(|m| m.as_str()).unwrap_or("");
    if first_str.is_empty() && last_str.is_empty() {
        return None;
    }
    let length = size as u128;
    let (start, end) = if first_str.is_empty() {
        let suffix: u128 = last_str.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let st = if suffix >= length {
            0
        } else {
            (length - suffix) as u64
        };
        (st, size - 1)
    } else {
        let first: u128 = first_str.parse().ok()?;
        let last: u128 = if last_str.is_empty() {
            length - 1
        } else {
            last_str.parse().ok()?
        };
        if first >= length || last < first {
            return None;
        }
        let en = if last >= length {
            size - 1
        } else {
            last as u64
        };
        (first as u64, en)
    };
    Some(ByteRange {
        start,
        end,
        partial: true,
    })
}

#[derive(Debug, Clone)]
pub struct BuiltMediaResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub fn build_media_response(
    file: &Path,
    mime: &str,
    origin: &str,
    range: Option<&str>,
) -> Result<BuiltMediaResponse, String> {
    let meta =
        fs::symlink_metadata(file).map_err(|_| "Managed media unavailable.".to_string())?;
    let size = meta.len();
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || size == 0
        || size > 9_007_199_254_740_991
    {
        return Err("Managed media unavailable.".to_string());
    }
    let mut headers = vec![
        ("Content-Type".to_string(), mime.to_string()),
        ("Cache-Control".to_string(), "no-store".to_string()),
        ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
        (
            "Access-Control-Allow-Origin".to_string(),
            origin.to_string(),
        ),
        ("Vary".to_string(), "Origin".to_string()),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
    ];
    let Some(selected) = byte_range(range, size) else {
        headers.push(("Content-Range".to_string(), format!("bytes */{}", size)));
        headers.push(("Content-Length".to_string(), "0".to_string()));
        return Ok(BuiltMediaResponse {
            status: 416,
            headers,
            body: Vec::new(),
        });
    };

    let len = (selected.end - selected.start + 1) as usize;
    headers.push(("Content-Length".to_string(), len.to_string()));
    if selected.partial {
        headers.push((
            "Content-Range".to_string(),
            format!("bytes {}-{}/{}", selected.start, selected.end, size),
        ));
    }

    let mut f = File::open(file).map_err(|_| "Managed media unavailable.".to_string())?;
    if selected.start > 0 {
        f.seek(SeekFrom::Start(selected.start))
            .map_err(|_| "Managed media unavailable.".to_string())?;
    }
    let mut body = vec![0u8; len];
    f.read_exact(&mut body)
        .map_err(|_| "Managed media unavailable.".to_string())?;

    Ok(BuiltMediaResponse {
        status: if selected.partial { 206 } else { 200 },
        headers,
        body,
    })
}
