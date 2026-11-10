use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;

pub const MUTTER_DESKTOPS: [&str; 10] = [
    "gnome",
    "gnome-classic",
    "gnome-flashback",
    "ubuntu",
    "pantheon",
    "budgie",
    "budgie-desktop",
    "unity",
    "x-cinnamon",
    "cinnamon",
];

pub const RELAUNCHED: &str = "CLIPBOARDLINE_BACKEND_RELAUNCHED";
pub const RELEASES_URL: &str = "https://github.com/orkunisitmak/clipboardline/releases";
pub const AUTOSTART_NAME: &str = "clipboardline.desktop";
pub const AUTOSTART_OWNER: &str = "X-ClipBoardLine-Autostart=true";

fn executable_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap())
}

fn safe_exec_arg_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_@%+=:,./-]+$").unwrap())
}

fn portal_response_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(/org/freedesktop/portal/desktop/request/[A-Za-z0-9_]+/([A-Za-z0-9_]+)): org\.freedesktop\.portal\.Request\.Response \(uint32 (\d+), (.*)\)$")
            .unwrap()
    })
}

fn portal_uri_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"'uri': <'((?:[^'\\]|\\.)*)'>").unwrap())
}

fn handle_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,20}$").unwrap())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinuxSessionInfo {
    pub session: String,
    pub backend: String,
    pub reason: String,
    pub desktop: String,
    pub desktops: Vec<String>,
    pub mutter: bool,
}

pub fn desktop_names(env: &HashMap<String, String>) -> Vec<String> {
    let raw = env
        .get("XDG_CURRENT_DESKTOP")
        .or_else(|| env.get("XDG_SESSION_DESKTOP"))
        .or_else(|| env.get("DESKTOP_SESSION"))
        .map(|s| s.as_str())
        .unwrap_or("");
    raw.split(':')
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .collect()
}

pub fn explicit_backend(argv: &[String]) -> Option<String> {
    for arg in argv {
        if let Some(rest) = arg.strip_prefix("--ozone-platform=") {
            if rest == "wayland" || rest == "x11" {
                return Some(rest.to_string());
            }
        }
    }
    None
}

pub fn linux_session(env: &HashMap<String, String>, argv: &[String]) -> LinuxSessionInfo {
    let desktops = desktop_names(env);
    let wayland = env
        .get("WAYLAND_DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
        || env.get("XDG_SESSION_TYPE").map(|v| v.as_str()) == Some("wayland");
    let xwayland = env.get("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false);
    let mutter = desktops
        .iter()
        .any(|name| MUTTER_DESKTOPS.contains(&name.as_str()));

    let requested = explicit_backend(argv).or_else(|| {
        env.get("CLIPBOARDLINE_DISPLAY_BACKEND")
            .filter(|v| *v == "wayland" || *v == "x11")
            .cloned()
    });

    let (backend, reason) = if let Some(req) = requested {
        let supported = if req == "x11" { xwayland } else { wayland };
        if supported {
            (req, "requested".to_string())
        } else if !wayland {
            ("x11".to_string(), "x11-session".to_string())
        } else if mutter && xwayland {
            ("x11".to_string(), "mutter-selection-bridge".to_string())
        } else {
            ("wayland".to_string(), "wayland-session".to_string())
        }
    } else if !wayland {
        ("x11".to_string(), "x11-session".to_string())
    } else if mutter && xwayland {
        ("x11".to_string(), "mutter-selection-bridge".to_string())
    } else {
        ("wayland".to_string(), "wayland-session".to_string())
    };

    LinuxSessionInfo {
        session: if wayland {
            "wayland".to_string()
        } else {
            "x11".to_string()
        },
        backend,
        reason,
        desktop: desktops.first().cloned().unwrap_or_default(),
        desktops,
        mutter,
    }
}

pub fn backend_relaunch_arguments(
    session: &LinuxSessionInfo,
    argv: &[String],
    env: &HashMap<String, String>,
) -> Option<Vec<String>> {
    if session.session != "wayland"
        || session.backend != "x11"
        || env.get(RELAUNCHED).map(|s| s.as_str()) == Some("1")
        || argv.iter().any(|a| a.starts_with("--ozone-platform="))
    {
        return None;
    }
    let mut out = argv.to_vec();
    out.push("--ozone-platform=x11".to_string());
    Some(out)
}

pub fn launch_command(
    env: &HashMap<String, String>,
    exec_path: &str,
    packaged: bool,
    app_path: Option<&str>,
) -> Vec<String> {
    if let (Some(appdir), Some(appimage)) = (env.get("APPDIR"), env.get("APPIMAGE")) {
        if Path::new(appdir).is_absolute() && Path::new(appimage).is_absolute() {
            let mut prefix = Path::new(appdir).to_path_buf().into_os_string();
            prefix.push(std::path::MAIN_SEPARATOR.to_string());
            if let Some(prefix_str) = prefix.to_str() {
                if exec_path.starts_with(prefix_str) {
                    return vec![appimage.clone()];
                }
            }
        }
    }
    if packaged {
        vec![exec_path.to_string()]
    } else if let Some(ap) = app_path {
        vec![exec_path.to_string(), ap.to_string()]
    } else {
        vec![exec_path.to_string()]
    }
}

pub fn find_executable(name: &str, env: &HashMap<String, String>) -> Option<String> {
    if !executable_name_re().is_match(name) {
        return None;
    }
    let path_var = env.get("PATH")?;
    for dir in path_var.split(':') {
        if dir.is_empty() || !Path::new(dir).is_absolute() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111) != 0 {
                return candidate.to_str().map(|s| s.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub struct LinuxTools {
    pub wl_paste: Option<String>,
    pub wl_copy: Option<String>,
    pub xclip: Option<String>,
    pub xdotool: Option<String>,
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
    pub gdbus: Option<String>,
    pub tesseract: Option<String>,
}

pub fn linux_tools(env: &HashMap<String, String>) -> LinuxTools {
    LinuxTools {
        wl_paste: find_executable("wl-paste", env),
        wl_copy: find_executable("wl-copy", env),
        xclip: find_executable("xclip", env),
        xdotool: find_executable("xdotool", env),
        ffmpeg: find_executable("ffmpeg", env),
        ffprobe: find_executable("ffprobe", env),
        gdbus: find_executable("gdbus", env),
        tesseract: find_executable("tesseract", env),
    }
}

pub fn map_animation_delay(session: &LinuxSessionInfo, env: &HashMap<String, String>) -> u64 {
    if !session.desktops.iter().any(|d| d == "kde") {
        return if session.mutter { 150 } else { 0 };
    }
    let mut factor = 1.0_f64;
    let config_home = env
        .get("XDG_CONFIG_HOME")
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env.get("HOME").map(|s| s.as_str()).unwrap_or("/");
            Path::new(home).join(".config")
        });
    if let Ok(content) = fs::read_to_string(config_home.join("kdeglobals")) {
        let mut section = String::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                section = trimmed.to_string();
            } else if section == "[KDE]" {
                if let Some(rest) = trimmed.strip_prefix("AnimationDurationFactor=") {
                    if let Ok(parsed) = rest.parse::<f64>() {
                        factor = parsed;
                    }
                }
            }
        }
    }
    if factor.is_finite() {
        (200.0 * factor).clamp(0.0, 600.0).round() as u64
    } else {
        200
    }
}

// Autostart helpers
pub fn autostart_file(env: &HashMap<String, String>) -> PathBuf {
    let base = env
        .get("XDG_CONFIG_HOME")
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env.get("HOME").map(|s| s.as_str()).unwrap_or("/");
            Path::new(home).join(".config")
        });
    base.join("autostart").join(AUTOSTART_NAME)
}

pub fn exec_argument(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value
            .chars()
            .any(|c| (c as u32) <= 0x1f || c == '\x7f' || ['"', '`', '$', '\\'].contains(&c))
    {
        return Err("This application path cannot be used in an autostart entry.".to_string());
    }
    let escaped = value.replace('%', "%%");
    if safe_exec_arg_re().is_match(&escaped) {
        Ok(escaped)
    } else {
        Ok(format!("\"{}\"", escaped))
    }
}

pub fn autostart_entry(command: &[String], name: Option<&str>) -> Result<String, String> {
    if command.is_empty() {
        return Err("Invalid autostart command.".to_string());
    }
    let raw_name = name.unwrap_or("ClipBoardLine");
    let clean_name: String = raw_name
        .chars()
        .map(|c| {
            if c == '\r' || c == '\n' || (c as u32) <= 0x1f || c == '\x7f' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut quoted_args = Vec::with_capacity(command.len());
    for arg in command {
        quoted_args.push(exec_argument(arg)?);
    }
    Ok([
        "[Desktop Entry]",
        "Type=Application",
        &format!("Name={}", clean_name),
        "Comment=Keyboard-first clipboard shelves",
        &format!("Exec={}", quoted_args.join(" ")),
        "Icon=clipboardline",
        "Terminal=false",
        "X-GNOME-Autostart-enabled=true",
        AUTOSTART_OWNER,
        "",
    ]
    .join("\n"))
}

pub fn set_autostart(
    enabled: bool,
    command: &[String],
    env: &HashMap<String, String>,
    name: Option<&str>,
) -> Result<bool, String> {
    let file = autostart_file(env);
    let directory = file
        .parent()
        .ok_or_else(|| "Invalid autostart directory.".to_string())?;
    let existing = fs::symlink_metadata(&file).ok();
    if let Some(ref meta) = existing {
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            return Err("The autostart entry is not a regular file.".to_string());
        }
        let content = fs::read_to_string(&file).map_err(|e| e.to_string())?;
        if !content.contains(AUTOSTART_OWNER) {
            return Err(format!(
                "Another autostart entry named {} already exists.",
                AUTOSTART_NAME
            ));
        }
    }
    if !enabled {
        if existing.is_some() {
            fs::remove_file(&file).map_err(|e| e.to_string())?;
        }
        return Ok(false);
    }
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let dir_meta = fs::symlink_metadata(directory).map_err(|e| e.to_string())?;
    if !dir_meta.file_type().is_dir() || dir_meta.file_type().is_symlink() {
        return Err("The autostart directory is not a regular directory.".to_string());
    }
    let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o700));
    let entry_text = autostart_entry(command, name)?;
    let temporary = directory.join(format!(".{}.{}", AUTOSTART_NAME, Uuid::new_v4()));
    let res = (|| -> Result<(), String> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        f.write_all(entry_text.as_bytes())
            .map_err(|e| e.to_string())?;
        drop(f);
        fs::rename(&temporary, &file).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    res?;
    Ok(true)
}

pub fn autostart_enabled(env: &HashMap<String, String>) -> bool {
    let file = autostart_file(env);
    let Ok(meta) = fs::symlink_metadata(&file) else {
        return false;
    };
    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return false;
    }
    fs::read_to_string(&file)
        .map(|c| c.contains(AUTOSTART_OWNER))
        .unwrap_or(false)
}

// Portal Screenshot helpers
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalResponse {
    pub code: u32,
    pub uri: Option<String>,
}

pub fn parse_portal_response(line: &str, token: &str) -> Option<PortalResponse> {
    let trimmed = line.trim();
    let caps = portal_response_re().captures(trimmed)?;
    if caps.get(2)?.as_str() != token {
        return None;
    }
    let code: u32 = caps.get(3)?.as_str().parse().ok()?;
    let details = caps.get(4)?.as_str();
    let uri = portal_uri_re().captures(details).and_then(|c| {
        let raw = c.get(1)?.as_str();
        let mut unescaped = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next_ch) = chars.next() {
                    unescaped.push(next_ch);
                }
            } else {
                unescaped.push(ch);
            }
        }
        Some(unescaped)
    });
    Some(PortalResponse { code, uri })
}

pub fn screenshot_path(response: Option<&PortalResponse>) -> Result<PathBuf, String> {
    let Some(resp) = response else {
        return Err("The screenshot portal did not respond.".to_string());
    };
    if resp.code == 1 {
        return Err("Screen capture was canceled.".to_string());
    }
    let Some(ref uri_str) = resp.uri else {
        return Err("The desktop screenshot portal could not capture the screen.".to_string());
    };
    if resp.code != 0 {
        return Err("The desktop screenshot portal could not capture the screen.".to_string());
    }
    let parsed = Url::parse(uri_str)
        .map_err(|_| "The screenshot portal returned an unsupported location.".to_string())?;
    let file = parsed
        .to_file_path()
        .map_err(|_| "The screenshot portal returned an unsupported location.".to_string())?;
    let meta = fs::symlink_metadata(&file)
        .map_err(|_| "The captured screenshot is unavailable.".to_string())?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.len() > 64 * 1024 * 1024
    {
        return Err("The captured screenshot is unavailable.".to_string());
    }
    Ok(file)
}

pub fn portal_screenshot(gdbus: Option<&str>, timeout_ms: u64) -> Result<PathBuf, String> {
    let Some(gdbus_bin) = gdbus else {
        return Err(
            "Screen capture on Wayland requires gdbus and xdg-desktop-portal.".to_string(),
        );
    };
    let rand_bytes: [u8; 8] = rand::random();
    let token = format!("clipboardline_{}", hex::encode(rand_bytes));

    let mut monitor = Command::new(gdbus_bin)
        .args([
            "monitor",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    let stdout = monitor.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let gdbus_owned = gdbus_bin.to_string();
    let token_owned = token.clone();

    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut called = false;
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if !called && line.starts_with("Monitoring signals") {
                called = true;
                let g = gdbus_owned.clone();
                let t = token_owned.clone();
                let tx_call = tx.clone();
                std::thread::spawn(move || {
                    let status = Command::new(&g)
                        .args([
                            "call",
                            "--session",
                            "--dest",
                            "org.freedesktop.portal.Desktop",
                            "--object-path",
                            "/org/freedesktop/portal/desktop",
                            "--method",
                            "org.freedesktop.portal.Screenshot.Screenshot",
                            "",
                            &format!("{{'handle_token': <'{}'>, 'interactive': <false>}}", t),
                        ])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    if !status.map(|s| s.success()).unwrap_or(false) {
                        let _ = tx_call.send(Err(
                            "The desktop screenshot portal is unavailable.".to_string()
                        ));
                    }
                });
                continue;
            }
            if let Some(resp) = parse_portal_response(&line, &token_owned) {
                let _ = tx.send(screenshot_path(Some(&resp)));
                return;
            }
        }
        let _ = tx.send(Err("The screenshot portal monitor stopped.".to_string()));
    });

    let res = match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(r) => r,
        Err(_) => Err("The screenshot portal did not respond.".to_string()),
    };
    let _ = monitor.kill();
    let _ = monitor.wait();
    res
}

pub fn x11_screenshot(
    ffmpeg: Option<&str>,
    display: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let Some(ffmpeg_bin) = ffmpeg else {
        return Err("Screen capture on X11 requires ffmpeg or xdg-desktop-portal.".to_string());
    };
    let size_arg = format!("{}x{}", width.max(1), height.max(1));
    let input_arg = format!("{}+{},{}", display, x.max(0), y.max(0));
    crate::linux_clipboard::read_process(
        ffmpeg_bin,
        &[
            "-loglevel",
            "error",
            "-f",
            "x11grab",
            "-video_size",
            &size_arg,
            "-i",
            &input_arg,
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ],
        64 * 1024 * 1024,
        10_000,
        None,
    )
    .map_err(|e| e.message)
}

// Paste targets & xdotool helpers
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasteTarget {
    pub handle: String,
    #[serde(rename = "processId")]
    pub process_id: u32,
}

pub fn valid_target(target: Option<&PasteTarget>, own_pid: u32) -> bool {
    let Some(t) = target else {
        return false;
    };
    handle_re().is_match(&t.handle)
        && t.handle != "0"
        && t.process_id > 0
        && t.process_id != own_pid
}

pub struct LinuxPasteTargets {
    pub own_pid: u32,
    pub external: Option<PasteTarget>,
}

impl LinuxPasteTargets {
    pub fn new(own_pid: u32) -> Self {
        Self {
            own_pid,
            external: None,
        }
    }

    pub fn capture(&mut self, value: Option<PasteTarget>) -> Option<PasteTarget> {
        if valid_target(value.as_ref(), self.own_pid) {
            self.external = value;
        } else {
            self.external = None;
        }
        self.external.clone()
    }

    pub fn reenter(&self) -> Option<PasteTarget> {
        self.external.clone()
    }

    pub fn resolve(&self, captured: Option<&PasteTarget>) -> Option<PasteTarget> {
        if valid_target(captured, self.own_pid) {
            captured.cloned()
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.external = None;
    }
}

pub fn run_short_command(
    file: &str,
    args: &[&str],
    timeout_ms: u64,
    active_children: Option<&Arc<Mutex<Vec<u32>>>>,
) -> Result<String, String> {
    let mut child = Command::new(file)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let pid = child.id();
    if let Some(children) = active_children {
        children.lock().unwrap().push(pid);
    }
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(64000).read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).trim().to_string());
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let res = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    break Ok(rx
                        .recv_timeout(Duration::from_millis(200))
                        .unwrap_or_default());
                } else {
                    break Err("Helper command failed.".to_string());
                }
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err("Helper command timed out.".to_string());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                break Err(e.to_string());
            }
        }
    };
    if let Some(children) = active_children {
        children.lock().unwrap().retain(|&id| id != pid);
    }
    res
}

pub fn xdotool_capture(
    xdotool: Option<&str>,
    own_pid: u32,
    children: Option<&Arc<Mutex<Vec<u32>>>>,
) -> Option<PasteTarget> {
    let xdotool_bin = xdotool?;
    let handle = run_short_command(xdotool_bin, &["getactivewindow"], 3000, children).ok()?;
    let pid_str =
        run_short_command(xdotool_bin, &["getwindowpid", &handle], 3000, children).ok()?;
    let process_id: u32 = pid_str.parse().ok()?;
    let target = PasteTarget { handle, process_id };
    if valid_target(Some(&target), own_pid) {
        Some(target)
    } else {
        None
    }
}

pub fn xdotool_restore(
    xdotool: Option<&str>,
    target: Option<&PasteTarget>,
    own_pid: u32,
    children: Option<&Arc<Mutex<Vec<u32>>>>,
) {
    if let (Some(xdotool_bin), Some(t)) = (xdotool, target) {
        if valid_target(Some(t), own_pid) {
            let _ = run_short_command(
                xdotool_bin,
                &["windowactivate", "--sync", &t.handle],
                3000,
                children,
            );
        }
    }
}

pub fn xdotool_paste(
    xdotool: Option<&str>,
    target: Option<&PasteTarget>,
    own_pid: u32,
    children: Option<&Arc<Mutex<Vec<u32>>>>,
) -> Result<bool, String> {
    let (Some(xdotool_bin), Some(t)) = (xdotool, target) else {
        return Ok(false);
    };
    if !valid_target(Some(t), own_pid) {
        return Ok(false);
    }
    let active = run_short_command(xdotool_bin, &["getactivewindow"], 3000, children)?;
    if active != t.handle {
        return Err(
            "The previous window is no longer active. The item was copied; press Ctrl+V to paste."
                .to_string(),
        );
    }
    run_short_command(
        xdotool_bin,
        &["key", "--clearmodifiers", "ctrl+v"],
        3000,
        children,
    )?;
    Ok(true)
}

pub fn linux_status_value(
    session: &LinuxSessionInfo,
    tools: &LinuxTools,
    clipboard_monitor: &str,
    problem: &str,
) -> Value {
    let wayland = session.backend == "wayland";
    let mut missing = Vec::new();
    if wayland && (tools.wl_paste.is_none() || tools.wl_copy.is_none()) {
        missing.push("wl-clipboard");
    }
    if tools.ffmpeg.is_none() || tools.ffprobe.is_none() {
        missing.push("ffmpeg");
    }
    if session.session == "wayland" && tools.gdbus.is_none() {
        missing.push("gdbus");
    }
    if session.backend == "x11" && tools.xclip.is_none() {
        missing.push("xclip");
    }
    if session.backend == "x11" && tools.xdotool.is_none() {
        missing.push("xdotool");
    }
    json!({
        "session": session.session,
        "backend": session.backend,
        "desktop": session.desktop,
        "clipboardMonitor": clipboard_monitor,
        "automaticPaste": session.backend == "x11" && tools.xdotool.is_some(),
        "screenshot": if session.session == "wayland" { "portal" } else { "x11" },
        "recordings": tools.ffmpeg.is_some() && tools.ffprobe.is_some(),
        "missing": missing,
        "problem": problem
    })
}

pub fn manual_updates_state(version: &str, build: u64) -> Value {
    json!({
        "status": "manual",
        "message": "Install newer Linux releases as an AppImage, .deb or .rpm package.",
        "error": null,
        "checkedAt": null,
        "lastAttemptAt": null,
        "currentVersion": version,
        "currentBuild": build,
        "busy": false,
        "canInstall": false,
        "availableVersion": null,
        "availableBuild": null,
        "releasesURL": RELEASES_URL
    })
}
