use crate::clipboard_policy::{private_clipboard, ClipboardEntry, ClipboardReader};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use url::Url;

pub const TEXT_LIMIT: usize = 1024 * 1024;
pub const IMAGE_LIMIT: usize = 64 * 1024 * 1024;
pub const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
pub const FORMAT_MAP: &str = "application/web;type=\"custom/formatmap\"";
pub const MARKER_TYPE: &str = "web application/x.clipboardline";
pub const MAX_DECODED_PIXELS: u64 = 80_000_000;

fn empty_clipboard_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)nothing is copied|no selection|clipboard content is not available|no suitable type|there is no owner|cannot convert .* to target|target \S+ not available")
            .unwrap()
    })
}

fn timestamp_type_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^TIMESTAMP$|format="TIMESTAMP""#).unwrap())
}

#[derive(Debug, Clone)]
pub struct ProcessError {
    pub message: String,
    pub empty: bool,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub fn read_process(
    file: &str,
    args: &[&str],
    limit: usize,
    timeout_ms: u64,
    input: Option<&[u8]>,
) -> Result<Vec<u8>, ProcessError> {
    let mut cmd = Command::new(file);
    cmd.args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| ProcessError {
        message: e.to_string(),
        empty: false,
    })?;

    if let Some(in_bytes) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(in_bytes);
        }
    }

    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let (tx_out, rx_out) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut overflow = false;
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = (limit + 1).saturating_sub(buf.len());
                    let take = n.min(room);
                    buf.extend_from_slice(&chunk[..take]);
                    if buf.len() > limit {
                        overflow = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx_out.send((buf, overflow));
    });

    let (tx_err, rx_err) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut err_buf = Vec::new();
        let _ = stderr.take(2000).read_to_end(&mut err_buf);
        let _ = tx_err.send(String::from_utf8_lossy(&err_buf).to_string());
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok((out_bytes, true)) = rx_out.try_recv() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(out_bytes);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let (out_bytes, _) = rx_out
                    .recv_timeout(Duration::from_millis(200))
                    .unwrap_or((Vec::new(), false));
                if status.success() || out_bytes.len() > limit {
                    return Ok(out_bytes);
                }
                let stderr_str = rx_err
                    .recv_timeout(Duration::from_millis(200))
                    .unwrap_or_default();
                let empty = empty_clipboard_re().is_match(&stderr_str);
                let msg = stderr_str.trim().chars().take(300).collect::<String>();
                return Err(ProcessError {
                    message: if msg.is_empty() {
                        "Clipboard helper failed.".to_string()
                    } else {
                        msg
                    },
                    empty,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcessError {
                        message: "Clipboard helper timed out.".to_string(),
                        empty: false,
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(ProcessError {
                    message: e.to_string(),
                    empty: false,
                });
            }
        }
    }
}

pub fn copy_process(
    file: &str,
    args: &[&str],
    input: &[u8],
    timeout_ms: u64,
) -> Result<(), String> {
    let mut child = Command::new(file)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        let data = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                } else {
                    return Err("Clipboard helper could not set the clipboard.".to_string());
                }
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Clipboard write timed out.".to_string());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(e.to_string());
            }
        }
    }
}

pub fn parse_type_lines(buffer: &[u8], limit: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(buffer);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() && trimmed.len() <= 512 && seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

pub fn marker_target(map: &Value, available: &HashSet<String>) -> Option<String> {
    let obj = map.as_object()?;
    for (key, val) in obj {
        if key != "application/x.clipboardline" && key != MARKER_TYPE {
            continue;
        }
        let s = val.as_str()?;
        if s.len() > 200 {
            continue;
        }
        let target = if s.starts_with("application/web") {
            s.to_string()
        } else {
            format!("application/web;type=\"{}\"", s)
        };
        if available.contains(&target) {
            return Some(target);
        }
    }
    None
}

#[derive(Clone)]
pub struct LinuxClipboardEntry {
    types: Vec<String>,
    available: HashSet<String>,
    alias: HashMap<String, String>,
    reader: Arc<dyn Fn(&str, usize) -> Result<Vec<u8>, ProcessError> + Send + Sync>,
}

impl ClipboardEntry for LinuxClipboardEntry {
    fn types(&self) -> Vec<String> {
        self.types.clone()
    }

    fn get_type(&self, mime: &str) -> Result<Vec<u8>, String> {
        let source = self
            .alias
            .get(mime)
            .map(|s| s.as_str())
            .unwrap_or(mime);
        if !self.available.contains(source) {
            return Err("Clipboard type is unavailable.".to_string());
        }
        let limit = if mime == "image/png" {
            IMAGE_LIMIT
        } else {
            TEXT_LIMIT
        };
        (self.reader)(source, limit).map_err(|e| e.message)
    }
}

#[derive(Clone)]
pub struct WaylandClipboard {
    pub wl_paste: String,
}

impl WaylandClipboard {
    pub fn new(wl_paste: impl Into<String>) -> Self {
        Self {
            wl_paste: wl_paste.into(),
        }
    }

    pub fn read_type(&self, mime: &str, limit: usize) -> Result<Vec<u8>, ProcessError> {
        let timeout = if limit > TEXT_LIMIT { 10_000 } else { 3_000 };
        read_process(
            &self.wl_paste,
            &["--no-newline", "--type", mime],
            limit,
            timeout,
            None,
        )
    }

    pub fn types(&self) -> Result<Vec<String>, ProcessError> {
        match read_process(&self.wl_paste, &["--list-types"], 65536, 3000, None) {
            Ok(bytes) => Ok(parse_type_lines(&bytes, 200)),
            Err(e) if e.empty => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

impl ClipboardReader for WaylandClipboard {
    type Entry = LinuxClipboardEntry;

    fn read(&self) -> Result<Vec<Self::Entry>, String> {
        let raw = self.types().map_err(|e| e.message)?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let available: HashSet<String> = raw.iter().cloned().collect();
        let mut alias = HashMap::new();
        let mut types = raw;
        if let Some(&text_mime) = TEXT_TYPES.iter().find(|&&t| available.contains(t)) {
            alias.insert("text/plain".to_string(), text_mime.to_string());
            if !types.iter().any(|t| t == "text/plain") {
                types.push("text/plain".to_string());
            }
        }
        if available.contains(FORMAT_MAP) {
            if let Ok(bytes) = self.read_type(FORMAT_MAP, 65536) {
                if let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(target) = marker_target(&parsed, &available) {
                        alias.insert(MARKER_TYPE.to_string(), target);
                        if !types.iter().any(|t| t == MARKER_TYPE) {
                            types.push(MARKER_TYPE.to_string());
                        }
                    }
                }
            }
        }
        let wl = self.clone();
        Ok(vec![LinuxClipboardEntry {
            types,
            available,
            alias,
            reader: Arc::new(move |mime, limit| wl.read_type(mime, limit)),
        }])
    }
}

#[derive(Clone)]
pub struct X11Clipboard {
    pub xclip: String,
}

impl X11Clipboard {
    pub fn new(xclip: impl Into<String>) -> Self {
        Self {
            xclip: xclip.into(),
        }
    }

    pub fn read_type(&self, mime: &str, limit: usize) -> Result<Vec<u8>, ProcessError> {
        let timeout = if limit > TEXT_LIMIT { 10_000 } else { 3_000 };
        read_process(
            &self.xclip,
            &["-selection", "clipboard", "-o", "-t", mime],
            limit,
            timeout,
            None,
        )
    }

    pub fn types(&self) -> Result<Vec<String>, ProcessError> {
        match read_process(
            &self.xclip,
            &["-selection", "clipboard", "-o", "-t", "TARGETS"],
            65536,
            3000,
            None,
        ) {
            Ok(bytes) => Ok(parse_type_lines(&bytes, 200)),
            Err(e) if e.empty => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

impl ClipboardReader for X11Clipboard {
    type Entry = LinuxClipboardEntry;

    fn read(&self) -> Result<Vec<Self::Entry>, String> {
        let raw = self.types().map_err(|e| e.message)?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let available: HashSet<String> = raw.iter().cloned().collect();
        let mut alias = HashMap::new();
        let mut types = raw;
        if let Some(&text_mime) = TEXT_TYPES.iter().find(|&&t| available.contains(t)) {
            alias.insert("text/plain".to_string(), text_mime.to_string());
            if !types.iter().any(|t| t == "text/plain") {
                types.push("text/plain".to_string());
            }
        }
        if available.contains(FORMAT_MAP) {
            if let Ok(bytes) = self.read_type(FORMAT_MAP, 65536) {
                if let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(target) = marker_target(&parsed, &available) {
                        alias.insert(MARKER_TYPE.to_string(), target);
                        if !types.iter().any(|t| t == MARKER_TYPE) {
                            types.push(MARKER_TYPE.to_string());
                        }
                    }
                }
            }
        }
        let xc = self.clone();
        Ok(vec![LinuxClipboardEntry {
            types,
            available,
            alias,
            reader: Arc::new(move |mime, limit| xc.read_type(mime, limit)),
        }])
    }
}

pub struct WaylandWatchers {
    wl_paste: String,
    children: Arc<Mutex<HashMap<String, Child>>>,
    closed: Arc<AtomicBool>,
    changed: Arc<dyn Fn(&str) + Send + Sync>,
    failed: Arc<dyn Fn(String, bool) + Send + Sync>,
}

impl WaylandWatchers {
    pub fn new(
        wl_paste: impl Into<String>,
        changed: Arc<dyn Fn(&str) + Send + Sync>,
        failed: Arc<dyn Fn(String, bool) + Send + Sync>,
    ) -> Self {
        Self {
            wl_paste: wl_paste.into(),
            children: Arc::new(Mutex::new(HashMap::new())),
            closed: Arc::new(AtomicBool::new(false)),
            changed,
            failed,
        }
    }

    pub fn update(&self, text: bool, images: bool) {
        for (kind, wanted) in [("text", text), ("image", images)] {
            let has_child = self.children.lock().unwrap().contains_key(kind);
            if wanted && !has_child {
                self.start(kind);
            } else if !wanted && has_child {
                if let Some(mut child) = self.children.lock().unwrap().remove(kind) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }

    fn start(&self, kind: &str) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut args = Vec::new();
        if kind == "image" {
            args.push("--type");
            args.push("image/png");
        }
        args.extend(["--watch", "echo", "clipboardline-change"]);

        let mut child = match Command::new(&self.wl_paste)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                (self.failed)(e.to_string(), true);
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let kind_owned = kind.to_string();
        self.children
            .lock()
            .unwrap()
            .insert(kind_owned.clone(), child);

        let changed = self.changed.clone();
        let kind_for_out = kind_owned.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim() == "clipboardline-change" {
                    changed(&kind_for_out);
                }
            }
        });

        let children = self.children.clone();
        let closed = self.closed.clone();
        let failed = self.failed.clone();
        let started = Instant::now();
        std::thread::spawn(move || {
            let mut err_buf = Vec::new();
            let _ = stderr.take(1000).read_to_end(&mut err_buf);
            let stderr_str = String::from_utf8_lossy(&err_buf).to_string();
            if closed.load(Ordering::SeqCst) {
                return;
            }
            let status = {
                let mut map = children.lock().unwrap();
                if let Some(mut c) = map.remove(&kind_owned) {
                    c.wait().ok()
                } else {
                    return;
                }
            };
            if closed.load(Ordering::SeqCst) {
                return;
            }
            let lower = stderr_str.to_ascii_lowercase();
            let unsupported = lower.contains("data-control")
                || lower.contains("watch mode")
                || started.elapsed() < Duration::from_millis(1000);
            let msg = if lower.contains("data-control") || lower.contains("watch mode") {
                "This compositor does not offer the Wayland data-control protocol.".to_string()
            } else {
                let code_suffix = status
                    .and_then(|s| s.code())
                    .filter(|&c| c != 0)
                    .map(|c| format!(" ({})", c))
                    .unwrap_or_default();
                format!("The wl-paste clipboard monitor stopped{}.", code_suffix)
            };
            failed(msg, unsupported);
        });
    }

    pub fn kill(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let mut map = self.children.lock().unwrap();
        for (_, mut child) in map.drain() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn clipboard_signature<R: ClipboardReader>(
    clipboard: &R,
    images: bool,
) -> Result<String, String> {
    let entries = clipboard.read()?;
    if entries.is_empty() {
        return Ok(String::new());
    }
    let mut hasher = Sha256::new();
    let all_types: Vec<String> = entries.iter().flat_map(|e| e.types()).collect();
    if private_clipboard(&all_types) {
        for entry in &entries {
            let serialized = serde_json::to_string(&entry.types()).unwrap_or_default();
            hasher.update(serialized.as_bytes());
        }
        return Ok(hex::encode(hasher.finalize()));
    }
    let mut stamped = false;
    for entry in &entries {
        let types = entry.types();
        let serialized = serde_json::to_string(&types).unwrap_or_default();
        hasher.update(serialized.as_bytes());
        if let Some(ts_mime) = types.iter().find(|t| timestamp_type_re().is_match(t)) {
            if let Ok(bytes) = entry.get_type(ts_mime) {
                if !bytes.is_empty() && bytes.iter().any(|&b| b != 0) {
                    hasher.update(&bytes);
                    stamped = true;
                }
            }
        }
        if !stamped && types.iter().any(|t| t == "text/plain") {
            if let Ok(bytes) = entry.get_type("text/plain") {
                if bytes.len() <= TEXT_LIMIT {
                    hasher.update(&bytes);
                } else {
                    hasher.update(bytes.len().to_string().as_bytes());
                }
            }
        }
        if images && !stamped && types.iter().any(|t| t == "image/png") {
            if let Ok(bytes) = entry.get_type("image/png") {
                if bytes.len() <= IMAGE_LIMIT {
                    hasher.update(&bytes);
                } else {
                    hasher.update(bytes.len().to_string().as_bytes());
                }
            }
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn decodable_png(bytes: &[u8]) -> bool {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return false;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as u64;
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as u64;
    width > 0 && height > 0 && width.saturating_mul(height) <= MAX_DECODED_PIXELS
}

pub fn pixel_fingerprint(bytes: &[u8]) -> Option<String> {
    if !decodable_png(bytes) {
        return None;
    }
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let raw = rgba.into_raw();
    if w == 0 || h == 0 || raw.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(&raw);
    Some(format!(
        "image:pixels:{}x{}:{}",
        w,
        h,
        hex::encode(hasher.finalize())
    ))
}

#[derive(Debug, Clone)]
pub struct KeptClipboard {
    pub id: String,
    pub kind: String,
    pub value: Vec<u8>,
    pub fingerprint: String,
}

pub fn kept_copy<'a>(
    kept: Option<&'a KeptClipboard>,
    fingerprint: &str,
    items: &[Value],
) -> Option<&'a KeptClipboard> {
    let k = kept?;
    if k.kind != "image" || k.fingerprint.is_empty() || k.fingerprint != fingerprint {
        return None;
    }
    let on_shelf = items.iter().any(|item| {
        item.get("id").and_then(|v| v.as_str()) == Some(k.id.as_str())
            && item.get("trashedAt").map(|v| v.is_null()).unwrap_or(true)
            && !item
                .get("hiddenFromShelf")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    });
    if on_shelf {
        Some(k)
    } else {
        None
    }
}

pub struct FileClipboardPayload {
    pub uri_list: String,
    pub gnome: String,
}

pub fn file_clipboard(files: &[PathBuf]) -> FileClipboardPayload {
    let uris: Vec<String> = files
        .iter()
        .filter_map(|p| Url::from_file_path(p).ok().map(|u| u.to_string()))
        .collect();
    FileClipboardPayload {
        uri_list: format!("{}\r\n", uris.join("\r\n")),
        gnome: format!("copy\n{}", uris.join("\n")),
    }
}

pub fn write_linux_clipboard(
    text: &str,
    png: Option<&[u8]>,
    files: &[PathBuf],
    wayland: bool,
    wl_copy: Option<&str>,
    wl_paste: Option<&str>,
    xclip: Option<&str>,
) -> Result<(), String> {
    let transfer = if !files.is_empty() {
        Some(file_clipboard(files))
    } else {
        None
    };
    let (primary_type, primary_bytes) = if let Some(ref tr) = transfer {
        ("text/uri-list", tr.uri_list.as_bytes().to_vec())
    } else if let Some(img) = png {
        ("image/png", img.to_vec())
    } else {
        ("text/plain;charset=utf-8", text.as_bytes().to_vec())
    };

    if wayland {
        let Some(wl_copy_bin) = wl_copy else {
            return Err(
                "Install wl-clipboard so ClipBoardLine can copy while its window is hidden."
                    .to_string(),
            );
        };
        copy_process(
            wl_copy_bin,
            &["--type", primary_type],
            &primary_bytes,
            5000,
        )?;
        if let Some(wl_paste_bin) = wl_paste {
            for _ in 0..20 {
                if let Ok(read_back) = read_process(
                    wl_paste_bin,
                    &["--no-newline", "--type", primary_type],
                    primary_bytes.len() + 1,
                    3000,
                    None,
                ) {
                    if read_back == primary_bytes {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        Ok(())
    } else {
        let Some(xclip_bin) = xclip else {
            return Err("Install xclip so ClipBoardLine can copy on X11.".to_string());
        };
        copy_process(
            xclip_bin,
            &["-selection", "clipboard", "-i", "-t", primary_type],
            &primary_bytes,
            5000,
        )?;
        Ok(())
    }
}
