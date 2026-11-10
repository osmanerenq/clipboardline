use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use url::Url;
use uuid::Uuid;

pub const KINDS: [&str; 3] = ["image", "text", "link"];
pub const TAG_INPUT_LIMIT: usize = 4096;
pub const MAXIMUM_VIDEO_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn managed_media_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}\.(png|mp4|mov)$").unwrap())
}

fn managed_png_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}\.png$").unwrap())
}

fn managed_video_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}\.(mp4|mov)$").unwrap())
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-f-]{36}$").unwrap())
}

fn local_shortcut_action_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]{0,60}$").unwrap())
}

fn mark_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\p{M}").unwrap())
}

fn cc_cf_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\p{Cc}\p{Cf}]").unwrap())
}

fn data_url_png_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^data:image/png;base64,[A-Za-z0-9+/=]+$").unwrap())
}

pub fn default_settings() -> Value {
    json!({
        "visibleKinds": ["image", "text", "link"],
        "captureText": false,
        "captureImages": false,
        "captureLinks": true,
        "fetchLinkMetadata": true,
        "retentionDays": { "image": 30, "text": 30, "link": 30 },
        "shortcuts": {
            "toggle": "Control+Alt+T",
            "search": "Control+Alt+S",
            "library": "",
            "copyLatest": "",
            "screenshot": "Control+Alt+A"
        },
        "localShortcuts": {},
        "autoCheckUpdates": true,
        "watchFolders": [],
        "openAfterScreenshot": true,
        "theme": "soft",
        "cardSize": "medium",
        "sounds": false,
        "launchAtLogin": false,
        "language": "system",
        "indexImages": true,
        "libraryListMode": false,
        "libraryInspectorVisible": true,
        "librarySidebarVisible": true,
        "librarySidebarWidth": 220,
        "libraryInspectorWidth": 300,
        "libraryListColumns": {
            "title": 300,
            "kind": 80,
            "tags": 150,
            "created": 130
        },
        "recentPinFolderIds": [],
        "lineFullScreen": false
    })
}

#[derive(Debug, Clone)]
pub struct ModelError {
    pub message: String,
    pub committed: bool,
}

impl ModelError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            committed: false,
        }
    }

    pub fn incomplete_deletion(msg: &str) -> Self {
        Self {
            message: format!(
                "Permanent deletion is incomplete. {} Retry cleanup in Settings.",
                msg
            ),
            committed: true,
        }
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ModelError {}

fn fail<T>(msg: &str) -> Result<T, ModelError> {
    Err(ModelError::new(msg))
}

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

pub fn text_value(value: Option<&Value>, maximum: usize, empty: bool) -> Result<String, ModelError> {
    let Some(s) = value.and_then(|v| v.as_str()) else {
        return fail("Invalid text");
    };
    validate_text_str(s, maximum, empty)
}

pub fn validate_text_str(s: &str, maximum: usize, empty: bool) -> Result<String, ModelError> {
    if utf16_len(s) > maximum || (!empty && s.is_empty()) || s.contains('\0') {
        return fail("Invalid text");
    }
    Ok(s.to_string())
}

pub fn sized_text(value: Option<&Value>, empty: bool) -> Result<String, ModelError> {
    let Some(s) = value.and_then(|v| v.as_str()) else {
        return fail("Invalid text");
    };
    validate_sized_text_str(s, empty)
}

pub fn validate_sized_text_str(s: &str, empty: bool) -> Result<String, ModelError> {
    if s.len() > 1024 * 1024 {
        return fail("Invalid text");
    }
    validate_text_str(s, 1024 * 1024, empty)
}

pub fn ids(value: Option<&Value>) -> Result<Vec<String>, ModelError> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return fail("Invalid item selection");
    };
    if arr.len() > 10000 {
        return fail("Invalid item selection");
    }
    let mut out = Vec::with_capacity(arr.len());
    let mut seen = HashSet::new();
    for entry in arr {
        let Some(id) = entry.as_str() else {
            return fail("Invalid item selection");
        };
        if !uuid_re().is_match(id) {
            return fail("Invalid item selection");
        }
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

pub fn text_identity(value: &str) -> String {
    let normalized: String = value.nfc().collect();
    let lf = normalized.replace("\r\n", "\n").replace('\r', "\n");
    lf.trim_matches('\n').to_string()
}

pub fn folded_name(value: &str) -> String {
    let mut compat = String::with_capacity(value.len());
    for ch in value.chars() {
        let code = ch as u32;
        if code == 0x3000 || (0x3131..=0x318e).contains(&code) || (0xff01..=0xffee).contains(&code)
        {
            if ch == '\u{ffe3}' {
                compat.push('\u{00af}');
            } else {
                for c in ch.nfkc() {
                    compat.push(c);
                }
            }
        } else {
            compat.push(ch);
        }
    }
    let nfd: String = compat.nfd().collect();
    let stripped = mark_re().replace_all(&nfd, "");
    stripped.to_lowercase()
}

pub fn valid_tag_name(name: &str) -> bool {
    !cc_cf_re().is_match(name) && name.graphemes(true).count() <= 64
}

pub fn plain_url(value: &str) -> Option<String> {
    let trimmed = value.trim_matches(char::is_whitespace);
    if trimmed.is_empty()
        || trimmed.chars().any(char::is_whitespace)
        || utf16_len(trimmed) > 8192
    {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("http://") {
        r
    } else if let Some(r) = lower.strip_prefix("https://") {
        r
    } else {
        return None;
    };
    if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
        return None;
    }
    let parsed = Url::parse(trimmed).ok()?;
    if (parsed.scheme() == "http" || parsed.scheme() == "https")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.host_str().is_some()
    {
        Some(parsed.to_string())
    } else {
        None
    }
}

pub fn saved(item: &Value) -> bool {
    let retained = item.get("retainedAt").map(|v| !v.is_null()).unwrap_or(false);
    let pinned = item.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false);
    let folders = item
        .get("folderIds")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let tags = item
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    retained || pinned || folders || tags
}

pub fn recording_metadata(value: &Value) -> Result<Map<String, Value>, ModelError> {
    let Some(obj) = value.as_object() else {
        return fail("Invalid recording metadata.");
    };
    let duration_ms = obj
        .get("durationMs")
        .and_then(|v| v.as_f64())
        .filter(|d| d.is_finite() && *d > 0.0 && *d <= 24.0 * 60.0 * 60.0 * 1000.0)
        .ok_or_else(|| ModelError::new("Invalid recording metadata."))?;
    let width = obj
        .get("width")
        .and_then(|v| v.as_i64())
        .filter(|w| *w >= 1)
        .ok_or_else(|| ModelError::new("Invalid recording metadata."))?;
    let height = obj
        .get("height")
        .and_then(|v| v.as_i64())
        .filter(|h| *h >= 1)
        .ok_or_else(|| ModelError::new("Invalid recording metadata."))?;
    if width.saturating_mul(height) > 80_000_000 {
        return fail("Invalid recording metadata.");
    }
    let video_codec: String = obj
        .get("videoCodec")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    let audio_tracks = obj.get("audioTracks").and_then(|v| v.as_i64()).unwrap_or(0);

    let mut out = Map::new();
    if duration_ms.fract() == 0.0 {
        out.insert("durationMs".to_string(), json!(duration_ms as i64));
    } else {
        out.insert("durationMs".to_string(), json!(duration_ms));
    }
    out.insert("width".to_string(), json!(width));
    out.insert("height".to_string(), json!(height));
    out.insert("videoCodec".to_string(), json!(video_codec));
    out.insert("audioTracks".to_string(), json!(audio_tracks));
    Ok(out)
}

pub fn validate_settings(patch: &Value, previous: Option<&Value>) -> Result<Value, ModelError> {
    let Some(patch_obj) = patch.as_object() else {
        return fail("Invalid settings");
    };
    let mut out = previous.cloned().unwrap_or_else(default_settings);
    let out_obj = out.as_object_mut().unwrap();

    let bool_keys = [
        "captureText",
        "captureImages",
        "captureLinks",
        "fetchLinkMetadata",
        "autoCheckUpdates",
        "openAfterScreenshot",
        "sounds",
        "launchAtLogin",
        "indexImages",
        "libraryListMode",
        "libraryInspectorVisible",
        "librarySidebarVisible",
        "lineFullScreen",
    ];

    for (key, value) in patch_obj {
        if bool_keys.contains(&key.as_str()) {
            let Some(b) = value.as_bool() else {
                return fail("Invalid preference");
            };
            out_obj.insert(key.clone(), json!(b));
        } else if key == "visibleKinds" {
            let Some(arr) = value.as_array() else {
                return fail("Invalid visible shelves");
            };
            let mut kinds = Vec::new();
            let mut seen = HashSet::new();
            for item in arr {
                let Some(k) = item.as_str() else {
                    return fail("Invalid visible shelves");
                };
                if !KINDS.contains(&k) {
                    return fail("Invalid visible shelves");
                }
                if seen.insert(k.to_string()) {
                    kinds.push(json!(k));
                }
            }
            out_obj.insert(key.clone(), Value::Array(kinds));
        } else if key == "retentionDays" {
            let Some(ret_obj) = value.as_object() else {
                return fail("Invalid history duration");
            };
            let target = out_obj
                .entry("retentionDays")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
            for (kind, days_val) in ret_obj {
                let Some(days) = days_val.as_i64() else {
                    return fail("Invalid history duration");
                };
                if !KINDS.contains(&kind.as_str()) || !(0..=3650).contains(&days) {
                    return fail("Invalid history duration");
                }
                target.insert(kind.clone(), json!(days));
            }
        } else if key == "shortcuts" {
            let Some(sc_obj) = value.as_object() else {
                return fail("Invalid shortcut");
            };
            let target = out_obj
                .entry("shortcuts")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
            let allowed = ["toggle", "search", "library", "copyLatest", "screenshot"];
            for (action, sc_val) in sc_obj {
                let Some(sc) = sc_val.as_str() else {
                    return fail("Invalid shortcut");
                };
                if !allowed.contains(&action.as_str())
                    || utf16_len(sc) > 80
                    || sc.contains(['\r', '\n', '\0'])
                {
                    return fail("Invalid shortcut");
                }
                target.insert(action.clone(), json!(sc));
            }
        } else if key == "localShortcuts" {
            let Some(lsc_obj) = value.as_object() else {
                return fail("Invalid local shortcut");
            };
            let target = out_obj
                .entry("localShortcuts")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
            for (action, sc_val) in lsc_obj {
                if !local_shortcut_action_re().is_match(action)
                    || ["constructor", "prototype", "__proto__"].contains(&action.as_str())
                {
                    return fail("Invalid local shortcut");
                }
                if sc_val.is_null() {
                    target.remove(action);
                    continue;
                }
                let Some(sc) = sc_val.as_str() else {
                    return fail("Invalid local shortcut");
                };
                if utf16_len(sc) > 80 || sc.contains(['\r', '\n', '\0']) {
                    return fail("Invalid local shortcut");
                }
                target.insert(action.clone(), json!(sc));
            }
        } else if key == "recentPinFolderIds" {
            let Some(arr) = value.as_array() else {
                return fail("Invalid recent folders");
            };
            if arr.len() > 8 {
                return fail("Invalid recent folders");
            }
            let validated = ids(Some(value))?;
            out_obj.insert(key.clone(), json!(validated));
        } else if key == "libraryListColumns" {
            let Some(col_obj) = value.as_object() else {
                return fail("Invalid library columns");
            };
            let target = out_obj
                .entry("libraryListColumns")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
            for (col, w_val) in col_obj {
                let limits = match col.as_str() {
                    "title" => Some((180, 1200)),
                    "kind" => Some((70, 400)),
                    "tags" => Some((80, 800)),
                    "created" => Some((100, 500)),
                    _ => None,
                };
                let (Some((low, high)), Some(width)) = (limits, w_val.as_i64()) else {
                    return fail("Invalid library column width");
                };
                if width < low || width > high {
                    return fail("Invalid library column width");
                }
                target.insert(col.clone(), json!(width));
            }
        } else if key == "librarySidebarWidth" || key == "libraryInspectorWidth" {
            let (low, high) = if key == "librarySidebarWidth" {
                (180, 320)
            } else {
                (270, 400)
            };
            let Some(width) = value.as_i64() else {
                return fail("Invalid library panel width");
            };
            if width < low || width > high {
                return fail("Invalid library panel width");
            }
            out_obj.insert(key.clone(), json!(width));
        } else if key == "theme" || key == "cardSize" || key == "language" {
            let Some(s) = value.as_str() else {
                return fail("Invalid appearance preference");
            };
            let valid = match key.as_str() {
                "theme" => ["soft", "neutral"].contains(&s),
                "cardSize" => ["small", "medium", "large"].contains(&s),
                "language" => [
                    "system", "en", "tr", "es", "fr", "pt-BR", "az", "zh-Hans", "ja", "ko",
                ]
                .contains(&s),
                _ => false,
            };
            if !valid {
                return fail("Invalid appearance preference");
            }
            out_obj.insert(key.clone(), json!(s));
        } else if key == "watchFolders" {
            let Some(arr) = value.as_array() else {
                return fail("Invalid watched folder");
            };
            if arr.len() > 20 {
                return fail("Invalid watched folder");
            }
            let mut folders = Vec::new();
            let mut seen = HashSet::new();
            for entry in arr {
                let Some(p) = entry.as_str() else {
                    return fail("Invalid watched folder");
                };
                if !Path::new(p).is_absolute() || utf16_len(p) > 4096 {
                    return fail("Invalid watched folder");
                }
                if seen.insert(p.to_string()) {
                    folders.push(json!(p));
                }
            }
            out_obj.insert(key.clone(), Value::Array(folders));
        } else {
            return fail("Unknown preference");
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub id: String,
    #[serde(rename = "trashedAt")]
    pub trashed_at: Option<i64>,
    #[serde(rename = "hiddenFromShelf")]
    pub hidden_from_shelf: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashUndoEntry {
    pub id: String,
    #[serde(rename = "beforeHiddenFromShelf")]
    pub before_hidden_from_shelf: bool,
    #[serde(rename = "beforeTrashedAt")]
    pub before_trashed_at: Option<i64>,
    #[serde(rename = "afterTrashedAt")]
    pub after_trashed_at: Option<i64>,
    #[serde(rename = "afterUpdatedAt")]
    pub after_updated_at: i64,
    #[serde(rename = "captureRevision")]
    pub capture_revision: Option<String>,
    pub fingerprint: String,
}

fn trash_undo_fingerprint(item: &Value) -> String {
    let serialized = serde_json::to_string(item).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    hex::encode(hasher.finalize())
}

fn trash_undo_matches(item: Option<&Value>, entry: &TrashUndoEntry) -> bool {
    let Some(item) = item else {
        return false;
    };
    let trashed_at = item.get("trashedAt").and_then(|v| v.as_i64());
    if trashed_at.is_none() || trashed_at != entry.after_trashed_at {
        return false;
    }
    let updated_at = item.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(-1);
    if updated_at != entry.after_updated_at {
        return false;
    }
    let capture_rev = item
        .get("captureRevision")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if capture_rev != entry.capture_revision {
        return false;
    }
    trash_undo_fingerprint(item) == entry.fingerprint
}

fn available_trash_undo(items: &[Value], journal: &[Vec<TrashUndoEntry>]) -> bool {
    if journal.is_empty() {
        return false;
    }
    let by_id: HashMap<&str, &Value> = items
        .iter()
        .filter_map(|item| item.get("id").and_then(|v| v.as_str()).map(|id| (id, item)))
        .collect();
    for batch in journal.iter().rev() {
        if batch
            .iter()
            .any(|entry| trash_undo_matches(by_id.get(entry.id.as_str()).copied(), entry))
        {
            return true;
        }
    }
    false
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub struct Store {
    pub directory: PathBuf,
    pub media_directory: PathBuf,
    pub file: PathBuf,
    pub data: Value,
    pub undo_stack: Vec<Vec<UndoEntry>>,
    pub trash_undo_stack: Vec<Vec<TrashUndoEntry>>,
    pub revision: u64,
    pub privacy_warning: String,
    pub in_flight_media_references: HashMap<String, usize>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl Store {
    pub fn new(directory: impl AsRef<Path>) -> Result<Self, ModelError> {
        Self::with_clock(directory, Box::new(now_millis))
    }

    pub fn with_clock(
        directory: impl AsRef<Path>,
        clock: Box<dyn Fn() -> i64 + Send + Sync>,
    ) -> Result<Self, ModelError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory).map_err(|e| ModelError::new(e.to_string()))?;
        let dir_meta =
            fs::symlink_metadata(&directory).map_err(|e| ModelError::new(e.to_string()))?;
        if dir_meta.file_type().is_symlink() {
            return fail("Application data must not be a symlink.");
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| ModelError::new(e.to_string()))?;

        let media_directory = directory.join("media");
        fs::create_dir_all(&media_directory).map_err(|e| ModelError::new(e.to_string()))?;
        let media_meta =
            fs::symlink_metadata(&media_directory).map_err(|e| ModelError::new(e.to_string()))?;
        if media_meta.file_type().is_symlink() {
            return fail("Managed media must not be a symlink.");
        }
        fs::set_permissions(&media_directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| ModelError::new(e.to_string()))?;

        let file = directory.join("library.json");
        let data = json!({
            "schema": 1,
            "items": [],
            "folders": [],
            "settings": default_settings()
        });

        let mut store = Self {
            directory,
            media_directory,
            file,
            data,
            undo_stack: Vec::new(),
            trash_undo_stack: Vec::new(),
            revision: 0,
            privacy_warning: String::new(),
            in_flight_media_references: HashMap::new(),
            clock,
        };

        if store.file.exists() {
            let file_meta =
                fs::symlink_metadata(&store.file).map_err(|e| ModelError::new(e.to_string()))?;
            if file_meta.file_type().is_symlink() {
                return fail("Library must not be a symlink.");
            }
            if file_meta.len() > 64 * 1024 * 1024 {
                return fail("Library exceeds safe load size");
            }
            let raw = fs::read_to_string(&store.file).map_err(|_| {
                ModelError::new(
                    "Library is unreadable. Original data was preserved; recover library.json.backup.",
                )
            })?;
            let mut parsed: Value = serde_json::from_str(&raw).map_err(|_| {
                ModelError::new(
                    "Library is unreadable. Original data was preserved; recover library.json.backup.",
                )
            })?;
            let schema = parsed.get("schema").and_then(|v| v.as_i64());
            let items_len = parsed
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| a.len());
            let folders_ok = parsed.get("folders").and_then(|v| v.as_array()).is_some();
            if schema != Some(1)
                || items_len.is_none()
                || items_len.unwrap() > 20000
                || !folders_ok
            {
                return fail("Unsupported library format");
            }
            let validated_settings =
                validate_settings(parsed.get("settings").unwrap_or(&json!({})), None)?;
            parsed
                .as_object_mut()
                .unwrap()
                .insert("settings".to_string(), validated_settings);

            let now = (store.clock)();
            if let Some(items) = parsed.get_mut("items").and_then(|v| v.as_array_mut()) {
                for item in items.iter_mut() {
                    Self::validate_stored(item, now)?;
                }
            }

            if let Some(pending) = parsed.get("pendingDeletionFiles") {
                if !pending.is_null() {
                    let Some(arr) = pending.as_array() else {
                        return fail("Invalid pending deletion state.");
                    };
                    if arr.len() > 200000
                        || arr.iter().any(|name| {
                            name.as_str()
                                .map(|s| !managed_media_re().is_match(s))
                                .unwrap_or(true)
                        })
                    {
                        return fail("Invalid pending deletion state.");
                    }
                }
            }
            store.data = parsed;
        }

        store.prune_recent_folders();
        let has_pending_backup = store
            .data
            .get("pendingBackupRefresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let has_pending_files = store
            .data
            .get("pendingDeletionFiles")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if has_pending_backup || has_pending_files {
            if let Err(err) = store.finish_permanent_deletion() {
                store.privacy_warning = err.message;
            }
        }
        store.expire(true)?;
        Ok(store)
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    fn validate_stored(item: &mut Value, now: i64) -> Result<(), ModelError> {
        let Some(obj) = item.as_object_mut() else {
            return fail("Invalid library record");
        };
        if let Some(cr) = obj.get("captureRevision").filter(|v| !v.is_null()) {
            ids(Some(&json!([cr.clone()])))?;
        }
        if let Some(tr) = obj.get("trashRevision").filter(|v| !v.is_null()) {
            ids(Some(&json!([tr.clone()])))?;
        }
        let id_val = obj.get("id").cloned().unwrap_or(Value::Null);
        ids(Some(&json!([id_val])))?;

        let kind = obj
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !KINDS.contains(&kind.as_str())
            || !obj.get("folderIds").map(|v| v.is_array()).unwrap_or(false)
            || !obj.get("tags").map(|v| v.is_array()).unwrap_or(false)
        {
            return fail("Invalid library record");
        }
        if let Some(t) = obj.get("text").filter(|v| !v.is_null()) {
            text_value(Some(t), 1024 * 1024, true)?;
        }
        if let Some(n) = obj.get("note").filter(|v| !v.is_null()) {
            text_value(Some(n), 1024 * 1024, true)?;
        }
        if let Some(cn) = obj.get("createdAsNote").filter(|v| !v.is_null()) {
            if !cn.is_boolean() {
                return fail("Invalid note marker");
            }
        }
        for (field, err_msg) in [
            ("retainedAt", "Invalid saved timestamp"),
            ("lastCopiedAt", "Invalid copy timestamp"),
            ("lastCapturedAt", "Invalid capture timestamp"),
        ] {
            if let Some(val) = obj.get(field).filter(|v| !v.is_null()) {
                let Some(num) = val.as_f64() else {
                    return fail(err_msg);
                };
                if !num.is_finite() || num < 0.0 {
                    return fail(err_msg);
                }
            }
        }
        let is_retained_null = obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true);
        let is_pinned = obj.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false);
        let has_folders = obj
            .get("folderIds")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let has_tags = obj
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if is_retained_null && (is_pinned || has_folders || has_tags) {
            let fallback = obj
                .get("updatedAt")
                .and_then(|v| v.as_i64())
                .or_else(|| obj.get("createdAt").and_then(|v| v.as_i64()))
                .unwrap_or(now);
            obj.insert("retainedAt".to_string(), json!(fallback));
        }
        for key in ["imageFile", "renderedFile", "thumbnailFile"] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() && !managed_png_re().is_match(s) {
                    return fail("Unsafe managed media path");
                }
            } else if let Some(v) = obj.get(key) {
                if !v.is_null() {
                    return fail("Unsafe managed media path");
                }
            }
        }
        if let Some(rec) = obj.get("recording") {
            if !rec.is_null() && !rec.is_boolean() {
                return fail("Invalid recording record");
            }
        }
        let is_recording = obj
            .get("recording")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_recording {
            let vf = obj.get("videoFile").and_then(|v| v.as_str()).unwrap_or("");
            let imf = obj.get("imageFile").and_then(|v| v.as_str()).unwrap_or("");
            if kind != "image" || vf.is_empty() || imf.is_empty() {
                return fail("Invalid recording record");
            }
            recording_metadata(&Value::Object(obj.clone()))?;
        }
        for key in ["videoFile", "renderedVideoFile"] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() && (!is_recording || !managed_video_re().is_match(s)) {
                    return fail("Unsafe recording path");
                }
            } else if let Some(v) = obj.get(key) {
                if !v.is_null() {
                    return fail("Unsafe recording path");
                }
            }
        }
        Ok(())
    }

    pub fn prune_recent_folders(&mut self) {
        let known: HashSet<String> = self
            .data
            .get("folders")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(settings) = self
            .data
            .get_mut("settings")
            .and_then(|v| v.as_object_mut())
        {
            let filtered: Vec<Value> = settings
                .get("recentPinFolderIds")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|id| {
                            id.as_str().map(|s| known.contains(s)).unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            settings.insert("recentPinFolderIds".to_string(), Value::Array(filtered));
        }
    }

    pub fn persist(&mut self) -> Result<(), ModelError> {
        let backup = PathBuf::from(format!("{}.backup", self.file.display()));
        for dest in [&self.file, &backup] {
            if let Ok(meta) = fs::symlink_metadata(dest) {
                if meta.file_type().is_symlink() {
                    return fail("Library files must not be symlinks.");
                }
            }
        }
        let temporary = PathBuf::from(format!("{}.tmp-{}", self.file.display(), Uuid::new_v4()));
        let serialized =
            serde_json::to_vec(&self.data).map_err(|e| ModelError::new(e.to_string()))?;
        let write_result = (|| -> Result<(), ModelError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|e| ModelError::new(e.to_string()))?;
            file.write_all(&serialized)
                .map_err(|e| ModelError::new(e.to_string()))?;
            file.sync_all()
                .map_err(|e| ModelError::new(e.to_string()))?;
            drop(file);
            if self.file.exists() {
                fs::copy(&self.file, &backup).map_err(|e| ModelError::new(e.to_string()))?;
            }
            fs::rename(&temporary, &self.file).map_err(|e| ModelError::new(e.to_string()))?;
            self.revision += 1;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    pub fn media_references(&self) -> HashSet<String> {
        let mut refs = HashSet::new();
        if let Some(items) = self.data.get("items").and_then(|v| v.as_array()) {
            for item in items {
                for key in [
                    "imageFile",
                    "renderedFile",
                    "thumbnailFile",
                    "videoFile",
                    "renderedVideoFile",
                ] {
                    if let Some(s) = item.get(key).and_then(|v| v.as_str()) {
                        if !s.is_empty() {
                            refs.insert(s.to_string());
                        }
                    }
                }
            }
        }
        refs
    }

    pub fn retain_in_flight_media(&mut self, filename: &str) {
        *self
            .in_flight_media_references
            .entry(filename.to_string())
            .or_insert(0) += 1;
    }

    pub fn release_in_flight_media(&mut self, files: &[String]) {
        for filename in files {
            if let Some(count) = self.in_flight_media_references.get_mut(filename) {
                if *count <= 1 {
                    self.in_flight_media_references.remove(filename);
                } else {
                    *count -= 1;
                }
            }
        }
    }

    pub fn media_references_for_capacity_cleanup(&self) -> Option<HashSet<String>> {
        let mut referenced = self.media_references();
        let backup = PathBuf::from(format!("{}.backup", self.file.display()));
        match fs::symlink_metadata(&backup) {
            Ok(meta) => {
                if !meta.file_type().is_file() || meta.len() > 64 * 1024 * 1024 {
                    return None;
                }
                let raw = fs::read_to_string(&backup).ok()?;
                let previous: Value = serde_json::from_str(&raw).ok()?;
                if previous.get("schema").and_then(|v| v.as_i64()) != Some(1) {
                    return None;
                }
                let items = previous.get("items").and_then(|v| v.as_array())?;
                if items.len() > 20000 {
                    return None;
                }
                for item in items {
                    let obj = item.as_object()?;
                    for key in [
                        "imageFile",
                        "renderedFile",
                        "thumbnailFile",
                        "videoFile",
                        "renderedVideoFile",
                    ] {
                        if let Some(val) = obj.get(key) {
                            if val.is_null() {
                                continue;
                            }
                            let s = val.as_str()?;
                            if s.is_empty() {
                                continue;
                            }
                            if !managed_media_re().is_match(s) {
                                return None;
                            }
                            referenced.insert(s.to_string());
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        for filename in self.in_flight_media_references.keys() {
            referenced.insert(filename.clone());
        }
        Some(referenced)
    }

    pub fn prepare_permanent_deletion(&mut self) -> Result<(), ModelError> {
        let meta = fs::symlink_metadata(&self.media_directory)
            .map_err(|e| ModelError::new(e.to_string()))?;
        if meta.file_type().is_symlink() {
            return fail("Managed media must not be a symlink.");
        }
        let mut referenced = self.media_references();
        for filename in self.in_flight_media_references.keys() {
            referenced.insert(filename.clone());
        }
        let mut pending: Vec<String> = self
            .data
            .get("pendingDeletionFiles")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let mut seen: HashSet<String> = pending.iter().cloned().collect();
        if let Ok(entries) = fs::read_dir(&self.media_directory) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if managed_media_re().is_match(name)
                        && !referenced.contains(name)
                        && seen.insert(name.to_string())
                    {
                        pending.push(name.to_string());
                    }
                }
            }
        }
        let obj = self.data.as_object_mut().unwrap();
        obj.insert("pendingDeletionFiles".to_string(), json!(pending));
        obj.insert("pendingBackupRefresh".to_string(), json!(true));
        Ok(())
    }

    pub fn refresh_private_backup(&self) -> Result<(), ModelError> {
        let destination = PathBuf::from(format!("{}.backup", self.file.display()));
        if let Ok(meta) = fs::symlink_metadata(&destination) {
            if meta.file_type().is_symlink() {
                return fail("Library backup must not be a symlink.");
            }
        }
        let temporary = PathBuf::from(format!("{}.tmp-{}", destination.display(), Uuid::new_v4()));
        let serialized =
            serde_json::to_vec(&self.data).map_err(|e| ModelError::new(e.to_string()))?;
        let res = (|| -> Result<(), ModelError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|e| ModelError::new(e.to_string()))?;
            file.write_all(&serialized)
                .map_err(|e| ModelError::new(e.to_string()))?;
            file.sync_all()
                .map_err(|e| ModelError::new(e.to_string()))?;
            drop(file);
            fs::rename(&temporary, &destination).map_err(|e| ModelError::new(e.to_string()))?;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        res
    }

    pub fn finish_permanent_deletion(&mut self) -> Result<(), ModelError> {
        let pending: Vec<String> = self
            .data
            .get("pendingDeletionFiles")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let pending_backup = self
            .data
            .get("pendingBackupRefresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if pending.is_empty() && !pending_backup {
            return Ok(());
        }
        let step = (|| -> Result<(), ModelError> {
            self.refresh_private_backup()?;
            let meta = fs::symlink_metadata(&self.media_directory)
                .map_err(|e| ModelError::new(e.to_string()))?;
            if meta.file_type().is_symlink() {
                return fail("Managed media directory changed.");
            }
            let mut referenced = self.media_references();
            for filename in self.in_flight_media_references.keys() {
                referenced.insert(filename.clone());
            }
            for name in &pending {
                if !managed_media_re().is_match(name) {
                    return fail("Invalid managed filename.");
                }
                if referenced.contains(name) {
                    continue;
                }
                match fs::remove_file(self.media_directory.join(name)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(ModelError::new(e.to_string())),
                }
            }
            {
                let obj = self.data.as_object_mut().unwrap();
                obj.insert("pendingDeletionFiles".to_string(), json!([]));
                obj.insert("pendingBackupRefresh".to_string(), json!(false));
            }
            if let Err(err) = self.persist() {
                let obj = self.data.as_object_mut().unwrap();
                obj.insert("pendingDeletionFiles".to_string(), json!(pending));
                obj.insert("pendingBackupRefresh".to_string(), json!(true));
                return Err(err);
            }
            self.privacy_warning.clear();
            Ok(())
        })();
        if step.is_err() {
            let problem = ModelError::incomplete_deletion(
                "The local backup or unused managed images could not be removed.",
            );
            self.privacy_warning = problem.message.clone();
            return Err(problem);
        }
        Ok(())
    }

    pub fn put_png(&self, buffer: &[u8]) -> Result<String, ModelError> {
        if buffer.len() < 24
            || buffer.len() > 64 * 1024 * 1024
            || &buffer[0..8] != b"\x89PNG\r\n\x1a\n"
        {
            return fail("Invalid PNG image");
        }
        let width = u32::from_be_bytes([buffer[16], buffer[17], buffer[18], buffer[19]]) as u64;
        let height = u32::from_be_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]) as u64;
        if width == 0 || height == 0 || width.saturating_mul(height) > 80_000_000 {
            return fail("Image is too large");
        }
        let mut hasher = Sha256::new();
        hasher.update(buffer);
        let filename = format!("{}.png", hex::encode(hasher.finalize()));
        let destination = self.media_directory.join(&filename);
        if !destination.exists() {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&destination)
                .map_err(|e| ModelError::new(e.to_string()))?;
            f.write_all(buffer)
                .map_err(|e| ModelError::new(e.to_string()))?;
        }
        Ok(filename)
    }

    pub fn put_video(
        &self,
        source: &Path,
        aborted: Option<&dyn Fn() -> bool>,
        mut on_managed_file: Option<&mut dyn FnMut(&str)>,
    ) -> Result<String, ModelError> {
        if !source.is_absolute() {
            return fail("Invalid recording file");
        }
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_ascii_lowercase()))
            .unwrap_or_default();
        if ext != ".mp4" && ext != ".mov" {
            return fail("Invalid recording file");
        }
        let media_meta = fs::symlink_metadata(&self.media_directory)
            .map_err(|e| ModelError::new(e.to_string()))?;
        if media_meta.file_type().is_symlink() {
            return fail("Unsafe managed media directory");
        }
        let before = fs::symlink_metadata(source)
            .map_err(|_| ModelError::new("Recording exceeds the 2 GB limit or is unavailable"))?;
        if !before.file_type().is_file()
            || before.file_type().is_symlink()
            || before.len() < 12
            || before.len() > MAXIMUM_VIDEO_BYTES
        {
            return fail("Recording exceeds the 2 GB limit or is unavailable");
        }
        let temporary = self
            .media_directory
            .join(format!(".video-{}", Uuid::new_v4()));

        let res = (|| -> Result<String, ModelError> {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(source)
                .map_err(|_| ModelError::new("Recording changed while importing"))?;
            let opened = input
                .metadata()
                .map_err(|_| ModelError::new("Recording changed while importing"))?;
            if opened.ino() != before.ino()
                || opened.len() != before.len()
                || opened.mtime() != before.mtime()
                || opened.mtime_nsec() != before.mtime_nsec()
            {
                return fail("Recording changed while importing");
            }

            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|e| ModelError::new(e.to_string()))?;

            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 1024 * 1024];
            let mut total: u64 = 0;
            loop {
                if aborted.map(|f| f()).unwrap_or(false) {
                    return fail("Video operation canceled");
                }
                let n = input
                    .read(&mut buf)
                    .map_err(|e| ModelError::new(e.to_string()))?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > MAXIMUM_VIDEO_BYTES {
                    return fail("Recording exceeds the 2 GB limit");
                }
                hasher.update(&buf[..n]);
                output
                    .write_all(&buf[..n])
                    .map_err(|_| ModelError::new("Recording could not be saved"))?;
            }

            let after = input
                .metadata()
                .map_err(|_| ModelError::new("Recording changed while importing"))?;
            let path_after = fs::symlink_metadata(source)
                .map_err(|_| ModelError::new("Recording changed while importing"))?;
            if total != before.len()
                || after.len() != before.len()
                || after.mtime() != before.mtime()
                || after.mtime_nsec() != before.mtime_nsec()
                || path_after.file_type().is_symlink()
                || path_after.ino() != before.ino()
                || path_after.mtime() != before.mtime()
                || path_after.mtime_nsec() != before.mtime_nsec()
            {
                return fail("Recording changed while importing");
            }

            output
                .sync_all()
                .map_err(|e| ModelError::new(e.to_string()))?;
            drop(output);
            if aborted.map(|f| f()).unwrap_or(false) {
                return fail("Video operation canceled");
            }
            let filename = format!("{}{}", hex::encode(hasher.finalize()), ext);
            let destination = self.media_directory.join(&filename);
            if let Some(cb) = on_managed_file.as_mut() {
                cb(&filename);
            }
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = fs::symlink_metadata(&destination)
                        .map_err(|_| ModelError::new("Managed recording is unsafe"))?;
                    if !existing.file_type().is_file()
                        || existing.file_type().is_symlink()
                        || existing.len() != total
                    {
                        return fail("Managed recording is unsafe");
                    }
                }
                Err(e) => return Err(ModelError::new(e.to_string())),
            }
            if aborted.map(|f| f()).unwrap_or(false) {
                return fail("Video operation canceled");
            }
            Ok(filename)
        })();

        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        res
    }

    pub fn item(&self, id: &str) -> Result<Value, ModelError> {
        let validated = ids(Some(&json!([id])))?;
        let target = &validated[0];
        let items = self
            .data
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ModelError::new("An item no longer exists"))?;
        items
            .iter()
            .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(target.as_str()))
            .cloned()
            .ok_or_else(|| ModelError::new("An item no longer exists"))
    }

    pub fn media_path(&self, id: &str, original: bool) -> Result<PathBuf, ModelError> {
        let item = self.item(id)?;
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let is_rec = item
            .get("recording")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let filename = if is_rec {
            if original {
                item.get("videoFile").and_then(|v| v.as_str()).unwrap_or("")
            } else {
                let rv = item
                    .get("renderedVideoFile")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !rv.is_empty() {
                    rv
                } else {
                    item.get("videoFile").and_then(|v| v.as_str()).unwrap_or("")
                }
            }
        } else if original {
            item.get("imageFile").and_then(|v| v.as_str()).unwrap_or("")
        } else {
            let rf = item
                .get("renderedFile")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !rf.is_empty() {
                rf
            } else {
                item.get("imageFile").and_then(|v| v.as_str()).unwrap_or("")
            }
        };
        let valid_name = if is_rec {
            managed_video_re().is_match(filename)
        } else {
            managed_png_re().is_match(filename)
        };
        if kind != "image" || filename.is_empty() || !valid_name {
            return fail("Media unavailable");
        }
        let result = self.media_directory.join(filename);
        let Ok(meta) = fs::symlink_metadata(&result) else {
            return fail("Image unavailable");
        };
        if meta.file_type().is_symlink() {
            return fail("Image unavailable");
        }
        Ok(result)
    }

    pub fn poster_path(&self, id: &str, original: bool) -> Result<PathBuf, ModelError> {
        let item = self.item(id)?;
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let filename = if original {
            item.get("imageFile").and_then(|v| v.as_str()).unwrap_or("")
        } else {
            let rf = item
                .get("renderedFile")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !rf.is_empty() {
                rf
            } else {
                item.get("imageFile").and_then(|v| v.as_str()).unwrap_or("")
            }
        };
        if kind != "image" || filename.is_empty() || !managed_png_re().is_match(filename) {
            return fail("Image poster unavailable");
        }
        let file = self.media_directory.join(filename);
        let Ok(meta) = fs::symlink_metadata(&file) else {
            return fail("Image poster unavailable");
        };
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            return fail("Image poster unavailable");
        }
        Ok(file)
    }

    pub fn thumbnail_path(&self, id: &str) -> Result<PathBuf, ModelError> {
        let item = self.item(id)?;
        let filename = item
            .get("thumbnailFile")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if filename.is_empty() || !managed_png_re().is_match(filename) {
            return fail("Thumbnail unavailable");
        }
        let file = self.media_directory.join(filename);
        let Ok(meta) = fs::symlink_metadata(&file) else {
            return fail("Thumbnail unavailable");
        };
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            return fail("Thumbnail unavailable");
        }
        Ok(file)
    }

    pub fn insert_text(
        &mut self,
        value: &str,
        title: Option<&str>,
        fresh: bool,
        created_as_note: bool,
    ) -> Result<Value, ModelError> {
        self.insert_internal("text", Some(value), None, title, fresh, created_as_note)
    }

    pub fn insert_link(&mut self, value: &str, title: Option<&str>) -> Result<Value, ModelError> {
        self.insert_internal("link", Some(value), None, title, false, false)
    }

    pub fn insert_image(
        &mut self,
        png_bytes: &[u8],
        title: Option<&str>,
    ) -> Result<Value, ModelError> {
        self.insert_internal("image", None, Some(png_bytes), title, false, false)
    }

    fn insert_internal(
        &mut self,
        kind: &str,
        str_val: Option<&str>,
        bytes_val: Option<&[u8]>,
        title: Option<&str>,
        fresh: bool,
        created_as_note: bool,
    ) -> Result<Value, ModelError> {
        if !KINDS.contains(&kind) {
            return fail("Invalid content kind");
        }
        let mut text_val = String::new();
        let mut url_val = String::new();
        let mut image_file = String::new();
        if kind == "image" {
            let bytes = bytes_val.ok_or_else(|| ModelError::new("Invalid PNG image"))?;
            image_file = self.put_png(bytes)?;
        } else if kind == "link" {
            let s = str_val.ok_or_else(|| ModelError::new("Invalid URL"))?;
            url_val = plain_url(s).ok_or_else(|| ModelError::new("Invalid URL"))?;
        } else {
            let s = str_val.ok_or_else(|| ModelError::new("Invalid text"))?;
            text_val = validate_sized_text_str(s, false)?;
        }

        let identity = if kind == "text" {
            Some(text_identity(&text_val))
        } else {
            None
        };

        let items = self
            .data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut matching: Vec<Value> = if fresh {
            Vec::new()
        } else {
            items
                .iter()
                .filter(|i| {
                    let i_kind = i.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                    let trashed = i.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
                    let note_flag = i
                        .get("createdAsNote")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if i_kind != kind || trashed || note_flag {
                        return false;
                    }
                    if kind == "text" {
                        let it_text = i.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        Some(text_identity(it_text)) == identity
                    } else if kind == "link" {
                        i.get("url").and_then(|v| v.as_str()) == Some(url_val.as_str())
                    } else {
                        let rec = i
                            .get("recording")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        !rec && i.get("imageFile").and_then(|v| v.as_str())
                            == Some(image_file.as_str())
                    }
                })
                .cloned()
                .collect()
        };

        matching.sort_by(|a, b| {
            let ub = b.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            let ua = a.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            ub.cmp(&ua)
        });

        let prior = matching.first().cloned();
        if prior.is_none() && items.len() >= 20000 {
            if kind == "image" {
                if let Some(referenced) = self.media_references_for_capacity_cleanup() {
                    if !referenced.contains(&image_file) && managed_media_re().is_match(&image_file)
                    {
                        let _ = fs::remove_file(self.media_directory.join(&image_file));
                    }
                }
            }
            return fail("Library item limit reached");
        }

        let now = self.now();
        let has_prior = prior.is_some();
        let mut item = prior.unwrap_or_else(|| {
            json!({
                "id": Uuid::new_v4().to_string(),
                "kind": kind,
                "text": text_val,
                "url": url_val,
                "title": "",
                "name": "",
                "pinned": false,
                "folderIds": [],
                "tags": [],
                "imageFile": image_file,
                "renderedFile": "",
                "ocrText": "",
                "annotations": [],
                "crop": null,
                "note": "",
                "retainedAt": null,
                "lastCopiedAt": null,
                "createdAt": now,
                "updatedAt": now,
                "trashedAt": null,
                "hiddenFromShelf": false
            })
        });

        {
            let obj = item.as_object_mut().unwrap();
            if kind == "link" && has_prior {
                obj.insert("lastCopiedAt".to_string(), json!(now));
                obj.insert("lastCapturedAt".to_string(), json!(now));
            } else {
                obj.insert("createdAt".to_string(), json!(now));
            }
            obj.insert("updatedAt".to_string(), json!(now));
            obj.insert("hiddenFromShelf".to_string(), json!(false));
            obj.insert(
                "captureRevision".to_string(),
                json!(Uuid::new_v4().to_string()),
            );
            if kind == "text" {
                obj.insert("text".to_string(), json!(text_val));
            }
            if let Some(t) = title {
                if !t.is_empty() {
                    let valid_title = validate_text_str(t, 500, true)?;
                    obj.insert("title".to_string(), json!(valid_title));
                }
            }
            if created_as_note {
                obj.insert("createdAsNote".to_string(), json!(true));
            }
            if matching.len() > 1 {
                let any_pinned = matching
                    .iter()
                    .any(|m| m.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false));
                let cur_pinned = obj.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false);
                obj.insert("pinned".to_string(), json!(cur_pinned || any_pinned));

                let mut folder_ids = Vec::new();
                let mut seen_f = HashSet::new();
                for m in &matching {
                    if let Some(arr) = m.get("folderIds").and_then(|v| v.as_array()) {
                        for fid in arr.iter().filter_map(|v| v.as_str()) {
                            if seen_f.insert(fid.to_string()) {
                                folder_ids.push(json!(fid));
                            }
                        }
                    }
                }
                obj.insert("folderIds".to_string(), Value::Array(folder_ids));

                let mut tags = Vec::new();
                let mut seen_t = HashSet::new();
                for m in &matching {
                    if let Some(arr) = m.get("tags").and_then(|v| v.as_array()) {
                        for t in arr.iter().filter_map(|v| v.as_str()) {
                            if seen_t.insert(t.to_string()) {
                                tags.push(json!(t));
                            }
                        }
                    }
                }
                obj.insert("tags".to_string(), Value::Array(tags));

                if obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
                {
                    let found_name = matching
                        .iter()
                        .filter_map(|m| m.get("name").and_then(|v| v.as_str()))
                        .find(|s| !s.is_empty())
                        .unwrap_or("");
                    obj.insert("name".to_string(), json!(found_name));
                }
                if obj
                    .get("note")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
                {
                    let found_note = matching
                        .iter()
                        .filter_map(|m| m.get("note").and_then(|v| v.as_str()))
                        .find(|s| !s.is_empty())
                        .unwrap_or("");
                    obj.insert("note".to_string(), json!(found_note));
                }
                let min_retained = matching
                    .iter()
                    .filter_map(|m| m.get("retainedAt").and_then(|v| v.as_i64()))
                    .min();
                if let Some(min_r) = min_retained {
                    obj.insert("retainedAt".to_string(), json!(min_r));
                }
            }
        }

        let matching_ids: HashSet<String> = matching
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        let item_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut new_items = Vec::with_capacity(items.len() + 1);
        new_items.push(item.clone());
        for existing in items {
            let eid = existing.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if eid != item_id && !matching_ids.contains(eid) {
                new_items.push(existing);
            }
        }
        self.data
            .as_object_mut()
            .unwrap()
            .insert("items".to_string(), Value::Array(new_items));
        self.expire(false)?;
        self.persist()?;
        Ok(item)
    }

    pub fn create_note(&mut self, text: Option<&str>, title: Option<&str>) -> Result<Value, ModelError> {
        let note_text = text.filter(|s| !s.is_empty()).unwrap_or("New note");
        let item = self.insert_text(note_text, None, true, true)?;
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(t) = title.filter(|s| !s.is_empty()) {
            self.modify("details", &json!({ "id": id, "name": t }))?;
        }
        self.modify("retain", &json!({ "ids": [id] }))?;
        self.item(&id)
    }

    fn recording_assets(
        &mut self,
        value: &Value,
        aborted: Option<&dyn Fn() -> bool>,
    ) -> Result<(Map<String, Value>, String, String, Vec<String>), ModelError> {
        let mut in_flight = Vec::new();
        let details = recording_metadata(value)?;
        let file_str = value
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModelError::new("Invalid recording file"))?;

        let mut newly_managed = None;
        let video_file = match self.put_video(
            Path::new(file_str),
            aborted,
            Some(&mut |fname| {
                newly_managed = Some(fname.to_string());
            }),
        ) {
            Ok(vf) => {
                if let Some(fname) = newly_managed {
                    self.retain_in_flight_media(&fname);
                    in_flight.push(fname);
                }
                vf
            }
            Err(e) => {
                if let Some(fname) = newly_managed {
                    self.retain_in_flight_media(&fname);
                    in_flight.push(fname);
                }
                self.release_in_flight_media(&in_flight);
                return Err(e);
            }
        };

        let step = (|| -> Result<(Map<String, Value>, String, String), ModelError> {
            let poster_bytes = if let Some(b64) = value.get("posterPNG").and_then(|v| v.as_str()) {
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|_| ModelError::new("Invalid recording poster"))?
            } else {
                let pf = value
                    .get("posterFile")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ModelError::new("Recording poster unavailable"))?;
                let ppath = Path::new(pf);
                if !ppath.is_absolute() {
                    return fail("Recording poster unavailable");
                }
                let meta = fs::symlink_metadata(ppath)
                    .map_err(|_| ModelError::new("Recording poster unavailable"))?;
                if !meta.file_type().is_file()
                    || meta.file_type().is_symlink()
                    || meta.len() > 4 * 1024 * 1024
                {
                    return fail("Recording poster unavailable");
                }
                fs::read(ppath).map_err(|_| ModelError::new("Recording poster unavailable"))?
            };
            if poster_bytes.len() > 4 * 1024 * 1024 {
                return fail("Invalid recording poster");
            }
            if aborted.map(|f| f()).unwrap_or(false) {
                return fail("Video operation canceled");
            }
            let poster_file = self.put_png(&poster_bytes)?;
            self.retain_in_flight_media(&poster_file);
            in_flight.push(poster_file.clone());
            Ok((details, video_file, poster_file))
        })();

        match step {
            Ok((d, vf, pf)) => Ok((d, vf, pf, in_flight)),
            Err(e) => {
                self.release_in_flight_media(&in_flight);
                Err(e)
            }
        }
    }

    pub fn insert_recording(
        &mut self,
        value: &Value,
        aborted: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, ModelError> {
        let (details, video_file, poster_file, mut in_flight) =
            self.recording_assets(value, aborted)?;
        let res = (|| -> Result<Value, ModelError> {
            let items = self
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut matching: Vec<Value> = items
                .iter()
                .filter(|item| {
                    item.get("recording")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        && item.get("trashedAt").map(|v| v.is_null()).unwrap_or(true)
                        && item.get("videoFile").and_then(|v| v.as_str())
                            == Some(video_file.as_str())
                })
                .cloned()
                .collect();
            matching.sort_by(|a, b| {
                let cb = b.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
                let ca = a.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
                cb.cmp(&ca)
            });
            let prior = matching.first().cloned();
            if aborted.map(|f| f()).unwrap_or(false) {
                return fail("Video operation canceled");
            }
            if prior.is_none() && items.len() >= 20000 {
                self.release_in_flight_media(&in_flight);
                in_flight.clear();
                if let Some(referenced) = self.media_references_for_capacity_cleanup() {
                    for filename in [&video_file, &poster_file] {
                        if managed_media_re().is_match(filename) && !referenced.contains(filename) {
                            let _ = fs::remove_file(self.media_directory.join(filename));
                        }
                    }
                }
                return fail("Library item limit reached");
            }

            let before = self.data.clone();
            let now = self.now();
            let mut item = prior.unwrap_or_else(|| {
                let mut base = json!({
                    "id": Uuid::new_v4().to_string(),
                    "kind": "image",
                    "recording": true,
                    "text": "",
                    "url": "",
                    "title": "",
                    "name": "",
                    "pinned": false,
                    "folderIds": [],
                    "tags": [],
                    "note": "",
                    "retainedAt": null,
                    "lastCopiedAt": null,
                    "createdAt": now,
                    "updatedAt": now,
                    "trashedAt": null,
                    "hiddenFromShelf": false,
                    "annotations": [],
                    "crop": null,
                    "ocrText": "",
                    "imageFile": poster_file,
                    "renderedFile": "",
                    "videoFile": video_file,
                    "renderedVideoFile": ""
                });
                let obj = base.as_object_mut().unwrap();
                for (k, v) in &details {
                    obj.insert(k.clone(), v.clone());
                }
                base
            });

            {
                let obj = item.as_object_mut().unwrap();
                obj.insert("createdAt".to_string(), json!(now));
                obj.insert("updatedAt".to_string(), json!(now));
                obj.insert("hiddenFromShelf".to_string(), json!(false));
                obj.insert(
                    "captureRevision".to_string(),
                    json!(Uuid::new_v4().to_string()),
                );
            }
            let item_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut new_items = Vec::with_capacity(items.len() + 1);
            new_items.push(item.clone());
            for entry in items {
                if entry.get("id").and_then(|v| v.as_str()) != Some(item_id.as_str()) {
                    new_items.push(entry);
                }
            }
            self.data
                .as_object_mut()
                .unwrap()
                .insert("items".to_string(), Value::Array(new_items));
            if let Err(e) = self.expire(false).and_then(|_| self.persist()) {
                self.data = before;
                return Err(e);
            }
            Ok(item)
        })();
        self.release_in_flight_media(&in_flight);
        res
    }

    pub fn commit_recording_trim(
        &mut self,
        value: &Value,
        aborted: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, ModelError> {
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModelError::new("An item no longer exists"))?;
        let expected = self.item(id)?;
        let is_rec = expected
            .get("recording")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let trashed = expected
            .get("trashedAt")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let Some(src_fp) = value.get("sourceFingerprint").and_then(|v| v.as_str()) else {
            return fail("The recording changed before trimming");
        };
        let cur_video = expected
            .get("renderedVideoFile")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| expected.get("videoFile").and_then(|v| v.as_str()))
            .unwrap_or("");
        if !is_rec || trashed || cur_video.len() < 64 || &cur_video[..64] != src_fp {
            return fail("The recording changed before trimming");
        }

        let (details, video_file, poster_file, in_flight) =
            self.recording_assets(value, aborted)?;
        let res = (|| -> Result<Value, ModelError> {
            let current = self.item(id)?;
            let is_rec2 = current
                .get("recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let trashed2 = current
                .get("trashedAt")
                .map(|v| !v.is_null())
                .unwrap_or(false);
            let cur_video2 = current
                .get("renderedVideoFile")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| current.get("videoFile").and_then(|v| v.as_str()))
                .unwrap_or("");
            if !is_rec2 || trashed2 || cur_video2.len() < 64 || &cur_video2[..64] != src_fp {
                return fail("The recording changed while trimming");
            }
            if aborted.map(|f| f()).unwrap_or(false) {
                return fail("Video operation canceled");
            }
            let before = self.data.clone();
            let now = self.now();
            let mut updated_item = Value::Null;
            if let Some(items) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
                for item in items.iter_mut() {
                    if item.get("id").and_then(|v| v.as_str()) == Some(id) {
                        let obj = item.as_object_mut().unwrap();
                        for (k, v) in &details {
                            obj.insert(k.clone(), v.clone());
                        }
                        obj.insert("renderedVideoFile".to_string(), json!(video_file));
                        obj.insert("renderedFile".to_string(), json!(poster_file));
                        obj.insert("updatedAt".to_string(), json!(now));
                        updated_item = item.clone();
                        break;
                    }
                }
            }
            if let Err(e) = self.persist() {
                self.data = before;
                return Err(e);
            }
            Ok(updated_item)
        })();
        self.release_in_flight_media(&in_flight);
        res
    }

    pub fn set_link_metadata(
        &mut self,
        id: &str,
        url: &str,
        title: Option<&str>,
        author: Option<&str>,
        thumbnail: Option<&[u8]>,
    ) -> bool {
        let mut thumbnail_file = String::new();
        if let Some(bytes) = thumbnail {
            if let Ok(f) = self.put_png(bytes) {
                thumbnail_file = f;
            }
        }
        let fallback_host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_default();
        let resolved_title = title
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or(fallback_host);
        let resolved_author = author.unwrap_or("").to_string();

        let mut found = false;
        if let Some(items) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
            for item in items.iter_mut() {
                let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let trashed = item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
                let item_url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
                if item_id == id && !trashed && item_url == url {
                    let obj = item.as_object_mut().unwrap();
                    obj.insert("title".to_string(), json!(resolved_title));
                    obj.insert("author".to_string(), json!(resolved_author));
                    if !thumbnail_file.is_empty() {
                        obj.insert("thumbnailFile".to_string(), json!(thumbnail_file));
                    }
                    found = true;
                    break;
                }
            }
        }
        if found {
            let _ = self.persist();
        }
        found
    }

    pub fn set_ocr_text(&mut self, id: &str, expected_file: &str, ocr_text: &str) -> bool {
        let mut found = false;
        if let Some(items) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
            for item in items.iter_mut() {
                let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let trashed = item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
                let cur_file = item
                    .get("renderedFile")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| item.get("imageFile").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if item_id == id && !trashed && cur_file == expected_file {
                    item.as_object_mut()
                        .unwrap()
                        .insert("ocrText".to_string(), json!(ocr_text));
                    found = true;
                    break;
                }
            }
        }
        if found {
            let _ = self.persist();
        }
        found
    }

    pub fn expire(&mut self, persist: bool) -> Result<bool, ModelError> {
        let now = self.now();
        let retention = self
            .data
            .get("settings")
            .and_then(|s| s.get("retentionDays"))
            .cloned()
            .unwrap_or_else(|| json!({ "image": 30, "text": 30, "link": 30 }));
        let mut changed = false;
        if let Some(items) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
            for item in items.iter_mut() {
                let trashed = item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
                if trashed || saved(item) {
                    continue;
                }
                let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("text");
                let days = retention.get(kind).and_then(|v| v.as_i64()).unwrap_or(30);
                if days > 0 {
                    let ts = item
                        .get("lastCapturedAt")
                        .and_then(|v| v.as_i64())
                        .or_else(|| item.get("createdAt").and_then(|v| v.as_i64()))
                        .unwrap_or(now);
                    if ts < now - days * 86_400_000 {
                        let obj = item.as_object_mut().unwrap();
                        obj.insert("trashedAt".to_string(), json!(now));
                        obj.insert(
                            "trashRevision".to_string(),
                            json!(Uuid::new_v4().to_string()),
                        );
                        changed = true;
                    }
                }
            }
        }
        if persist && changed {
            self.persist()?;
        }
        Ok(changed)
    }

    pub fn unsaved_history_snapshot(&self, kind: Option<&str>) -> Result<Value, ModelError> {
        let kind = kind.unwrap_or("all");
        if kind != "all" && !KINDS.contains(&kind) {
            return fail("Invalid history type");
        }
        let items = self
            .data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let filtered: Vec<&Value> = items
            .iter()
            .filter(|item| {
                let trashed = item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
                let ikind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                !trashed && !saved(item) && (kind == "all" || ikind == kind)
            })
            .collect();
        let mut sorted_ids: Vec<String> = filtered
            .iter()
            .filter_map(|item| item.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        sorted_ids.sort();
        let mut captures = Map::new();
        for item in filtered {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                captures.insert(
                    id.to_string(),
                    json!({
                        "revision": item.get("captureRevision").cloned().unwrap_or(Value::Null),
                        "createdAt": item.get("createdAt").cloned().unwrap_or(Value::Null),
                        "updatedAt": item.get("updatedAt").cloned().unwrap_or(Value::Null),
                        "lastCopiedAt": item.get("lastCopiedAt").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
        }
        Ok(json!({
            "kind": kind,
            "ids": sorted_ids,
            "captures": Value::Object(captures)
        }))
    }

    fn invalidate_trash_undo(&mut self, affected: &HashSet<String>) {
        self.trash_undo_stack = self
            .trash_undo_stack
            .drain(..)
            .map(|batch| {
                batch
                    .into_iter()
                    .filter(|entry| !affected.contains(&entry.id))
                    .collect::<Vec<_>>()
            })
            .filter(|batch| !batch.is_empty())
            .collect();
    }

    fn selected_indices(&self, value: Option<&Value>) -> Result<Vec<usize>, ModelError> {
        let selected_ids = ids(value)?;
        let items = self
            .data
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ModelError::new("An item no longer exists"))?;
        let by_id: HashMap<&str, usize> = items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| item.get("id").and_then(|v| v.as_str()).map(|id| (id, idx)))
            .collect();
        let mut indices = Vec::with_capacity(selected_ids.len());
        for id in &selected_ids {
            let Some(&idx) = by_id.get(id.as_str()) else {
                return fail("An item no longer exists");
            };
            indices.push(idx);
        }
        Ok(indices)
    }

    pub fn modify(&mut self, action: &str, payload: &Value) -> Result<Value, ModelError> {
        let before = self.data.clone();
        let undo = self.undo_stack.clone();
        let trash_undo = self.trash_undo_stack.clone();
        match self.modify_transaction(action, payload) {
            Ok(v) => Ok(v),
            Err(e) => {
                if !e.committed {
                    self.data = before;
                    self.undo_stack = undo;
                    self.trash_undo_stack = trash_undo;
                }
                Err(e)
            }
        }
    }

    fn modify_transaction(&mut self, action: &str, payload: &Value) -> Result<Value, ModelError> {
        let now = self.now();
        let id_arg = payload.get("ids").cloned().or_else(|| {
            payload
                .get("id")
                .filter(|v| !v.is_null())
                .map(|v| json!([v]))
        });

        if action == "trash" || action == "trashUndoably" {
            let selected = ids(id_arg.as_ref())?;
            let items = self
                .data
                .get_mut("items")
                .and_then(|v| v.as_array_mut())
                .unwrap();
            let by_id: HashMap<String, usize> = items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| (s.to_string(), idx))
                })
                .collect();
            let target_indices: Vec<usize> = selected
                .iter()
                .filter_map(|id| by_id.get(id).copied())
                .filter(|&idx| {
                    items[idx]
                        .get("trashedAt")
                        .map(|v| v.is_null())
                        .unwrap_or(true)
                })
                .collect();
            if target_indices.is_empty() {
                return Ok(json!({ "ids": [] }));
            }
            let mut batch = Vec::with_capacity(target_indices.len());
            let mut affected_ids = Vec::with_capacity(target_indices.len());
            for &idx in &target_indices {
                let item = &mut items[idx];
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let before_hidden = item
                    .get("hiddenFromShelf")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let before_trashed = item.get("trashedAt").and_then(|v| v.as_i64());
                {
                    let obj = item.as_object_mut().unwrap();
                    obj.insert("trashedAt".to_string(), json!(now));
                    obj.insert(
                        "trashRevision".to_string(),
                        json!(Uuid::new_v4().to_string()),
                    );
                    obj.insert("hiddenFromShelf".to_string(), json!(true));
                    obj.insert("updatedAt".to_string(), json!(now));
                }
                let capture_rev = item
                    .get("captureRevision")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let fp = trash_undo_fingerprint(item);
                batch.push(TrashUndoEntry {
                    id: id.clone(),
                    before_hidden_from_shelf: before_hidden,
                    before_trashed_at: before_trashed,
                    after_trashed_at: Some(now),
                    after_updated_at: now,
                    capture_revision: capture_rev,
                    fingerprint: fp,
                });
                affected_ids.push(id);
            }
            self.persist()?;
            let affected_set: HashSet<String> = affected_ids.iter().cloned().collect();
            self.undo_stack = self
                .undo_stack
                .drain(..)
                .map(|b| {
                    b.into_iter()
                        .filter(|e| !affected_set.contains(&e.id))
                        .collect::<Vec<_>>()
                })
                .filter(|b| !b.is_empty())
                .collect();
            self.invalidate_trash_undo(&affected_set);
            self.trash_undo_stack.push(batch);
            if self.trash_undo_stack.len() > 80 {
                self.trash_undo_stack.remove(0);
            }
            return Ok(json!({ "ids": affected_ids }));
        }

        if action == "undoTrash" {
            let items = self
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let by_id: HashMap<&str, &Value> = items
                .iter()
                .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(|id| (id, i)))
                .collect();
            let mut match_index = None;
            let mut matched_entries = Vec::new();
            for (idx, batch) in self.trash_undo_stack.iter().enumerate().rev() {
                let valid: Vec<TrashUndoEntry> = batch
                    .iter()
                    .filter(|e| trash_undo_matches(by_id.get(e.id.as_str()).copied(), e))
                    .cloned()
                    .collect();
                if !valid.is_empty() {
                    match_index = Some(idx);
                    matched_entries = valid;
                    break;
                }
            }
            let Some(idx) = match_index else {
                self.trash_undo_stack.clear();
                return Ok(json!({ "ids": [] }));
            };
            let items_mut = self
                .data
                .get_mut("items")
                .and_then(|v| v.as_array_mut())
                .unwrap();
            let mut restored_ids = Vec::with_capacity(matched_entries.len());
            for entry in &matched_entries {
                if let Some(item) = items_mut
                    .iter_mut()
                    .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(entry.id.as_str()))
                {
                    let obj = item.as_object_mut().unwrap();
                    obj.insert("trashedAt".to_string(), json!(entry.before_trashed_at));
                    obj.insert("trashRevision".to_string(), Value::Null);
                    obj.insert(
                        "hiddenFromShelf".to_string(),
                        json!(entry.before_hidden_from_shelf),
                    );
                    obj.insert("updatedAt".to_string(), json!(now));
                    restored_ids.push(entry.id.clone());
                }
            }
            self.persist()?;
            self.trash_undo_stack.truncate(idx);
            return Ok(json!({ "ids": restored_ids }));
        }

        if action == "clearUnsavedHistory" {
            let Some(raw_ids) = payload.get("ids").and_then(|v| v.as_array()) else {
                return fail("Invalid history snapshot");
            };
            if raw_ids.len() > 20000 {
                return fail("Invalid history snapshot");
            }
            let mut captured = HashSet::new();
            for chunk in raw_ids.chunks(10000) {
                for id in ids(Some(&Value::Array(chunk.to_vec())))? {
                    captured.insert(id);
                }
            }
            let Some(captures_arg) = payload.get("captures").and_then(|v| v.as_object()) else {
                return fail("Invalid history capture versions");
            };
            let kind = payload.get("kind").and_then(|v| v.as_str());
            let current = self.unsaved_history_snapshot(kind)?;
            let eligible: HashSet<String> = current
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let current_captures = current.get("captures").and_then(|v| v.as_object()).unwrap();

            let items_mut = self
                .data
                .get_mut("items")
                .and_then(|v| v.as_array_mut())
                .unwrap();
            let mut removed = Vec::new();
            let mut removed_set = HashSet::new();
            for item in items_mut.iter_mut() {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !captured.contains(&id) || !eligible.contains(&id) {
                    continue;
                }
                let (Some(a), Some(b)) = (captures_arg.get(&id), current_captures.get(&id)) else {
                    continue;
                };
                if a.get("revision") == b.get("revision")
                    && a.get("createdAt") == b.get("createdAt")
                    && a.get("updatedAt") == b.get("updatedAt")
                    && a.get("lastCopiedAt") == b.get("lastCopiedAt")
                {
                    let obj = item.as_object_mut().unwrap();
                    obj.insert("trashedAt".to_string(), json!(now));
                    obj.insert(
                        "trashRevision".to_string(),
                        json!(Uuid::new_v4().to_string()),
                    );
                    obj.insert("hiddenFromShelf".to_string(), json!(true));
                    obj.insert("updatedAt".to_string(), json!(now));
                    removed.push(id.clone());
                    removed_set.insert(id);
                }
            }
            if removed.is_empty() {
                return Ok(json!({ "ids": [] }));
            }
            self.undo_stack = self
                .undo_stack
                .drain(..)
                .map(|b| {
                    b.into_iter()
                        .filter(|e| !removed_set.contains(&e.id))
                        .collect::<Vec<_>>()
                })
                .filter(|b| !b.is_empty())
                .collect();
            self.persist()?;
            self.invalidate_trash_undo(&removed_set);
            return Ok(json!({ "ids": removed }));
        }

        if action == "cleanup" {
            self.finish_permanent_deletion()?;
            return Ok(Value::Null);
        }

        if action == "setSettings" {
            let patch = payload.get("patch").unwrap_or(&Value::Null);
            let current_settings = self.data.get("settings");
            let updated = validate_settings(patch, current_settings)?;
            self.data
                .as_object_mut()
                .unwrap()
                .insert("settings".to_string(), updated);
            self.prune_recent_folders();
            self.expire(false)?;
            self.persist()?;
            return Ok(Value::Null);
        }

        if action == "createFolder" {
            let raw_name = text_value(payload.get("name"), TAG_INPUT_LIMIT, false)?;
            let name = raw_name.trim().to_string();
            if name.is_empty() {
                return fail("Folder name is empty");
            }
            if !valid_tag_name(&name) {
                return fail("Invalid folder name");
            }
            let parent_id = payload
                .get("parentId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let folders = self
                .data
                .get("folders")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if let Some(ref pid) = parent_id {
                let Some(parent) = folders
                    .iter()
                    .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()))
                else {
                    return fail("Parent folder is unavailable");
                };
                if parent
                    .get("parentId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .is_some()
                {
                    return fail("Only top-level folders can contain subfolders");
                }
            }
            let folded = folded_name(&name);
            if folders.iter().any(|f| {
                let fp = f
                    .get("parentId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let fnm = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
                fp == parent_id && folded_name(fnm) == folded
            }) {
                return fail("A folder with this name already exists");
            }
            let folder = json!({
                "id": Uuid::new_v4().to_string(),
                "name": name,
                "parentId": parent_id
            });
            self.data
                .get_mut("folders")
                .and_then(|v| v.as_array_mut())
                .unwrap()
                .push(folder.clone());
            self.persist()?;
            return Ok(folder);
        }

        if action == "renameFolder" || action == "removeFolder" {
            let target_id = payload
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ModelError::new("Folder unavailable"))?;
            let folders = self
                .data
                .get("folders")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let Some(folder_idx) = folders
                .iter()
                .position(|f| f.get("id").and_then(|v| v.as_str()) == Some(target_id))
            else {
                return fail("Folder unavailable");
            };

            let original_memberships: HashMap<String, Value> = self
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|i| {
                            let id = i.get("id").and_then(|v| v.as_str())?.to_string();
                            let fids = i.get("folderIds").cloned().unwrap_or_else(|| json!([]));
                            Some((id, fids))
                        })
                        .collect()
                })
                .unwrap_or_default();

            if action == "renameFolder" {
                let raw_name = text_value(payload.get("name"), TAG_INPUT_LIMIT, false)?;
                let name = raw_name.trim().to_string();
                if name.is_empty() {
                    return fail("Folder name is empty");
                }
                if !valid_tag_name(&name) {
                    return fail("Invalid folder name");
                }
                let parent_id = folders[folder_idx]
                    .get("parentId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let folded = folded_name(&name);
                if folders.iter().enumerate().any(|(idx, f)| {
                    if idx == folder_idx {
                        return false;
                    }
                    let fp = f
                        .get("parentId")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    let fnm = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    fp == parent_id && folded_name(fnm) == folded
                }) {
                    return fail("A folder with this name already exists");
                }
                self.data
                    .get_mut("folders")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[folder_idx]
                    .as_object_mut()
                    .unwrap()
                    .insert("name".to_string(), json!(name));
            } else {
                let mut removed = HashSet::new();
                removed.insert(target_id.to_string());
                let mut changed = true;
                while changed {
                    changed = false;
                    for child in &folders {
                        let cid = child.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let pid = child.get("parentId").and_then(|v| v.as_str()).unwrap_or("");
                        if removed.contains(pid) && !removed.contains(cid) {
                            removed.insert(cid.to_string());
                            changed = true;
                        }
                    }
                }
                let remaining_folders: Vec<Value> = folders
                    .into_iter()
                    .filter(|f| {
                        let fid = f.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        !removed.contains(fid)
                    })
                    .collect();
                self.data
                    .as_object_mut()
                    .unwrap()
                    .insert("folders".to_string(), Value::Array(remaining_folders));
                if let Some(items) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
                    for item in items.iter_mut() {
                        let fids: Vec<String> = item
                            .get("folderIds")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if fids.iter().any(|id| removed.contains(id)) {
                            let obj = item.as_object_mut().unwrap();
                            if obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                                obj.insert("retainedAt".to_string(), json!(now));
                            }
                            let filtered: Vec<Value> = fids
                                .into_iter()
                                .filter(|id| !removed.contains(id))
                                .map(|id| json!(id))
                                .collect();
                            obj.insert("folderIds".to_string(), Value::Array(filtered));
                        }
                    }
                }
            }
            self.prune_recent_folders();
            self.persist()?;
            let changed_items: HashSet<String> = self
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let id = item.get("id").and_then(|v| v.as_str())?;
                            if original_memberships.get(id) != item.get("folderIds") {
                                Some(id.to_string())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.invalidate_trash_undo(&changed_items);
            return Ok(Value::Null);
        }

        if action == "renameTag" || action == "deleteTag" {
            let tag = text_value(payload.get("tag"), TAG_INPUT_LIMIT, false)?
                .trim()
                .to_string();
            let items = self
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let has_affected = !tag.is_empty()
                && items.iter().any(|item| {
                    item.get("tags")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().any(|t| t.as_str() == Some(tag.as_str())))
                        .unwrap_or(false)
                });
            if !has_affected {
                return fail("Tag unavailable");
            }
            let mut new_name = String::new();
            if action == "renameTag" {
                new_name = text_value(payload.get("name"), TAG_INPUT_LIMIT, false)?
                    .trim()
                    .to_string();
                if new_name.is_empty() || !valid_tag_name(&new_name) {
                    return fail("Invalid tag name");
                }
                let folded_new = folded_name(&new_name);
                if items.iter().any(|item| {
                    item.get("tags")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter().filter_map(|v| v.as_str()).any(|val| {
                                val != tag && folded_name(val) == folded_new
                            })
                        })
                        .unwrap_or(false)
                }) {
                    return fail("A tag with this name already exists");
                }
            }
            let mut affected_ids = HashSet::new();
            if let Some(items_mut) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
                for item in items_mut.iter_mut() {
                    let tags: Vec<String> = item
                        .get("tags")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if tags.contains(&tag) {
                        let id = item
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        affected_ids.insert(id);
                        let obj = item.as_object_mut().unwrap();
                        if obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                            obj.insert("retainedAt".to_string(), json!(now));
                        }
                        let updated_tags: Vec<Value> = if action == "renameTag" {
                            let mut seen = HashSet::new();
                            let mut out = Vec::new();
                            for t in tags {
                                let mapped = if t == tag { new_name.clone() } else { t };
                                if seen.insert(mapped.clone()) {
                                    out.push(json!(mapped));
                                }
                            }
                            out
                        } else {
                            tags.into_iter()
                                .filter(|t| t != &tag)
                                .map(|t| json!(t))
                                .collect()
                        };
                        obj.insert("tags".to_string(), Value::Array(updated_tags));
                    }
                }
            }
            self.persist()?;
            self.invalidate_trash_undo(&affected_ids);
            return Ok(Value::Null);
        }

        if action == "undo" {
            let Some(batch) = self.undo_stack.pop() else {
                return Ok(Value::Null);
            };
            if let Some(items_mut) = self.data.get_mut("items").and_then(|v| v.as_array_mut()) {
                for before in batch {
                    if let Some(item) = items_mut
                        .iter_mut()
                        .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(before.id.as_str()))
                    {
                        let obj = item.as_object_mut().unwrap();
                        obj.insert("trashedAt".to_string(), json!(before.trashed_at));
                        let tr = if before.trashed_at.is_none() {
                            Value::Null
                        } else {
                            json!(Uuid::new_v4().to_string())
                        };
                        obj.insert("trashRevision".to_string(), tr);
                        obj.insert("hiddenFromShelf".to_string(), json!(before.hidden_from_shelf));
                    }
                }
            }
            self.persist()?;
            return Ok(Value::Null);
        }

        let indices = self.selected_indices(id_arg.as_ref())?;
        if indices.is_empty() {
            return fail("Select an item first");
        }

        if action == "remove" {
            let items = self.data.get("items").and_then(|v| v.as_array()).unwrap();
            let batch: Vec<UndoEntry> = indices
                .iter()
                .map(|&idx| {
                    let i = &items[idx];
                    UndoEntry {
                        id: i
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        trashed_at: i.get("trashedAt").and_then(|v| v.as_i64()),
                        hidden_from_shelf: i
                            .get("hiddenFromShelf")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    }
                })
                .collect();
            self.undo_stack.push(batch);
            if self.undo_stack.len() > 32 {
                self.undo_stack.remove(0);
            }
        }

        let mut known_tags: HashMap<String, String> = HashMap::new();
        if action == "tag" {
            if let Some(items) = self.data.get("items").and_then(|v| v.as_array()) {
                for other in items {
                    if let Some(arr) = other.get("tags").and_then(|v| v.as_array()) {
                        for t in arr.iter().filter_map(|v| v.as_str()) {
                            let key = folded_name(t);
                            known_tags.entry(key).or_insert_with(|| t.to_string());
                        }
                    }
                }
            }
        }

        let target_pin = if let Some(b) = payload.get("value").and_then(|v| v.as_bool()) {
            b
        } else {
            let items = self.data.get("items").and_then(|v| v.as_array()).unwrap();
            indices.iter().any(|&idx| {
                !items[idx]
                    .get("pinned")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
        };

        let folder_exists = |fid: &str, data: &Value| -> bool {
            data.get("folders")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|f| f.get("id").and_then(|v| v.as_str()) == Some(fid))
                })
                .unwrap_or(false)
        };

        for &idx in &indices {
            let cur_item = self.data.get("items").and_then(|v| v.as_array()).unwrap()[idx].clone();
            let item_id = cur_item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let item_kind = cur_item
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if action == "pin" {
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                obj.insert("pinned".to_string(), json!(target_pin));
                if target_pin && obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                    obj.insert("retainedAt".to_string(), json!(now));
                }
            } else if action == "retain" {
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                if obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                    obj.insert("retainedAt".to_string(), json!(now));
                }
            } else if action == "note" {
                let note = sized_text(payload.get("note"), true)?;
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                item.as_object_mut()
                    .unwrap()
                    .insert("note".to_string(), json!(note));
            } else if action == "details" {
                let new_name = if let Some(n_val) = payload.get("name") {
                    Some(text_value(Some(n_val), 200, true)?.trim().to_string())
                } else {
                    None
                };
                let new_note = if let Some(nt_val) = payload.get("note") {
                    Some(sized_text(Some(nt_val), true)?)
                } else {
                    None
                };
                let new_text = if let Some(tx_val) = payload.get("text") {
                    if item_kind != "text" {
                        return fail("Only text can be edited");
                    }
                    Some(sized_text(Some(tx_val), true)?)
                } else {
                    None
                };
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                if let Some(n) = new_name {
                    obj.insert("name".to_string(), json!(n));
                }
                if let Some(nt) = new_note {
                    obj.insert("note".to_string(), json!(nt));
                }
                if let Some(tx) = new_text {
                    obj.insert("text".to_string(), json!(tx));
                }
            } else if action == "markCopied" {
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                obj.insert("lastCopiedAt".to_string(), json!(now));
                obj.insert(
                    "captureRevision".to_string(),
                    json!(Uuid::new_v4().to_string()),
                );
            } else if action == "name" {
                let name = text_value(payload.get("name"), 200, true)?
                    .trim()
                    .to_string();
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                obj.insert("name".to_string(), json!(name));
                obj.insert("pinned".to_string(), json!(true));
                if obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                    obj.insert("retainedAt".to_string(), json!(now));
                }
            } else if action == "folder" {
                let Some(folder_id) = payload.get("folderId").and_then(|v| v.as_str()) else {
                    return fail("Folder unavailable");
                };
                if !folder_exists(folder_id, &self.data) {
                    return fail("Folder unavailable");
                }
                if let Some(p) = payload.get("pinning") {
                    if !p.is_null() && !p.is_boolean() {
                        return fail("Invalid folder pin option");
                    }
                }
                if let Some(r) = payload.get("rememberRecent") {
                    if !r.is_null() && !r.is_boolean() {
                        return fail("Invalid recent folder option");
                    }
                }
                let pinning = payload
                    .get("pinning")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                let mut fids: Vec<Value> = obj
                    .get("folderIds")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !fids.iter().any(|v| v.as_str() == Some(folder_id)) {
                    fids.push(json!(folder_id));
                }
                obj.insert("folderIds".to_string(), Value::Array(fids));
                if pinning {
                    obj.insert("pinned".to_string(), json!(true));
                }
                if obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                    obj.insert("retainedAt".to_string(), json!(now));
                }
            } else if action == "unassign" {
                let Some(folder_id) = payload.get("folderId").and_then(|v| v.as_str()) else {
                    return fail("Folder unavailable");
                };
                if !folder_exists(folder_id, &self.data) {
                    return fail("Folder unavailable");
                }
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                let fids: Vec<Value> = obj
                    .get("folderIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|v| v.as_str() != Some(folder_id))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                obj.insert("folderIds".to_string(), Value::Array(fids));
            } else if action == "removeTag" {
                let tag = text_value(payload.get("tag"), TAG_INPUT_LIMIT, false)?
                    .trim()
                    .to_string();
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                let tags: Vec<Value> = obj
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|v| v.as_str() != Some(tag.as_str()))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                obj.insert("tags".to_string(), Value::Array(tags));
            } else if action == "tag" {
                let Some(raw_tags) = payload.get("tags").and_then(|v| v.as_array()) else {
                    return fail("Invalid tags");
                };
                if raw_tags.len() > 50 {
                    return fail("Invalid tags");
                }
                let own_tags: Vec<String> = cur_item
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut resolved_tags = Vec::new();
                let mut seen = HashSet::new();
                for t_val in raw_tags {
                    let mut t = text_value(Some(t_val), TAG_INPUT_LIMIT, false)?
                        .trim()
                        .to_string();
                    let final_tag = if own_tags.contains(&t) {
                        t
                    } else {
                        t = t.trim_matches('#').trim().to_string();
                        if t.is_empty() {
                            continue;
                        }
                        let key = folded_name(&t);
                        let known = own_tags
                            .iter()
                            .find(|own| folded_name(own) == key)
                            .cloned()
                            .or_else(|| known_tags.get(&key).cloned())
                            .unwrap_or_else(|| t.clone());
                        known_tags.insert(key, known.clone());
                        known
                    };
                    if !final_tag.is_empty() && seen.insert(final_tag.clone()) {
                        resolved_tags.push(final_tag);
                    }
                }
                if resolved_tags
                    .iter()
                    .any(|t| !own_tags.contains(t) && !valid_tag_name(t))
                {
                    return fail("Invalid tag name");
                }
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                let has_any = !resolved_tags.is_empty();
                obj.insert("tags".to_string(), json!(resolved_tags));
                if has_any && obj.get("retainedAt").map(|v| v.is_null()).unwrap_or(true) {
                    obj.insert("retainedAt".to_string(), json!(now));
                }
            } else if action == "remove" {
                let is_saved = saved(&cur_item);
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                if is_saved {
                    obj.insert("hiddenFromShelf".to_string(), json!(true));
                } else {
                    obj.insert("trashedAt".to_string(), json!(now));
                    obj.insert(
                        "trashRevision".to_string(),
                        json!(Uuid::new_v4().to_string()),
                    );
                }
            } else if action == "restore" {
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                obj.insert("trashedAt".to_string(), Value::Null);
                obj.insert("trashRevision".to_string(), Value::Null);
                obj.insert("hiddenFromShelf".to_string(), json!(false));
            } else if action == "editText" {
                if item_kind != "text" {
                    return fail("Only text can be edited");
                }
                let new_text = sized_text(payload.get("text"), true)?;
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                item.as_object_mut()
                    .unwrap()
                    .insert("text".to_string(), json!(new_text));
            } else if action == "saveImage" {
                let is_rec = cur_item
                    .get("recording")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let Some(data_url) = payload.get("dataUrl").and_then(|v| v.as_str()) else {
                    return fail("Invalid image edit");
                };
                if item_kind != "image"
                    || is_rec
                    || utf16_len(data_url) > 90_000_000
                    || !data_url_png_re().is_match(data_url)
                {
                    return fail("Invalid image edit");
                }
                let annotations = payload
                    .get("annotations")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let crop = payload.get("crop").cloned().unwrap_or(Value::Null);
                let Some(ann_arr) = annotations.as_array() else {
                    return fail("Invalid image edit metadata");
                };
                let ann_json_len = serde_json::to_string(&annotations)
                    .map(|s| utf16_len(&s))
                    .unwrap_or(usize::MAX);
                if ann_arr.len() > 2000 || ann_json_len > 2_000_000 {
                    return fail("Invalid image edit metadata");
                }
                if !crop.is_null() {
                    let Some(crop_obj) = crop.as_object() else {
                        return fail("Invalid image edit metadata");
                    };
                    let cx = crop_obj.get("x").and_then(|v| v.as_f64());
                    let cy = crop_obj.get("y").and_then(|v| v.as_f64());
                    let cw = crop_obj.get("width").and_then(|v| v.as_f64());
                    let ch = crop_obj.get("height").and_then(|v| v.as_f64());
                    let (Some(x), Some(y), Some(w), Some(h)) = (cx, cy, cw, ch) else {
                        return fail("Invalid image edit metadata");
                    };
                    if !x.is_finite()
                        || !y.is_finite()
                        || !w.is_finite()
                        || !h.is_finite()
                        || x < 0.0
                        || y < 0.0
                        || w <= 0.0
                        || h <= 0.0
                    {
                        return fail("Invalid image edit metadata");
                    }
                    let orig_path = self.media_path(&item_id, true)?;
                    let orig_bytes = fs::read(&orig_path)
                        .map_err(|_| ModelError::new("Image unavailable"))?;
                    if orig_bytes.len() < 24 {
                        return fail("Image unavailable");
                    }
                    let orig_w = u32::from_be_bytes([
                        orig_bytes[16],
                        orig_bytes[17],
                        orig_bytes[18],
                        orig_bytes[19],
                    ]) as f64;
                    let orig_h = u32::from_be_bytes([
                        orig_bytes[20],
                        orig_bytes[21],
                        orig_bytes[22],
                        orig_bytes[23],
                    ]) as f64;
                    if x + w > orig_w || y + h > orig_h {
                        return fail("Crop is outside the original image");
                    }
                }
                let b64_part = data_url.split(',').nth(1).unwrap_or("");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(b64_part)
                    .map_err(|_| ModelError::new("Invalid image edit"))?;
                let rendered_file = self.put_png(&decoded)?;
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                let obj = item.as_object_mut().unwrap();
                obj.insert("renderedFile".to_string(), json!(rendered_file));
                obj.insert("annotations".to_string(), annotations);
                obj.insert("crop".to_string(), crop);
                obj.insert("ocrText".to_string(), json!(""));
            } else if action == "delete" {
                let trashed = cur_item
                    .get("trashedAt")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
                if !trashed {
                    return fail("Move items to Trash before permanent deletion");
                }
            } else {
                return fail("Unknown library action");
            }

            if action != "markCopied" {
                let item = &mut self
                    .data
                    .get_mut("items")
                    .and_then(|v| v.as_array_mut())
                    .unwrap()[idx];
                item.as_object_mut()
                    .unwrap()
                    .insert("updatedAt".to_string(), json!(now));
            }
        }

        if action == "folder"
            && payload
                .get("rememberRecent")
                .and_then(|v| v.as_bool())
                == Some(true)
        {
            if let Some(folder_id) = payload.get("folderId").and_then(|v| v.as_str()) {
                let settings = self
                    .data
                    .get_mut("settings")
                    .and_then(|v| v.as_object_mut())
                    .unwrap();
                let mut recent: Vec<Value> = Vec::with_capacity(8);
                recent.push(json!(folder_id));
                if let Some(arr) = settings
                    .get("recentPinFolderIds")
                    .and_then(|v| v.as_array())
                {
                    for entry in arr {
                        if entry.as_str() != Some(folder_id) && recent.len() < 8 {
                            recent.push(entry.clone());
                        }
                    }
                }
                settings.insert("recentPinFolderIds".to_string(), Value::Array(recent));
            }
        }

        let affected_ids: HashSet<String> = {
            let items = self.data.get("items").and_then(|v| v.as_array()).unwrap();
            indices
                .iter()
                .filter_map(|&idx| {
                    items[idx]
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        };

        if action == "delete" {
            let items = self
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let remaining: Vec<Value> = items
                .into_iter()
                .filter(|i| {
                    let id = i.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    !affected_ids.contains(id)
                })
                .collect();
            self.data
                .as_object_mut()
                .unwrap()
                .insert("items".to_string(), Value::Array(remaining));
        }

        if ["delete", "trash", "restore"].contains(&action) {
            self.undo_stack = self
                .undo_stack
                .drain(..)
                .filter(|batch| !batch.iter().any(|old| affected_ids.contains(&old.id)))
                .collect();
        }

        if action == "delete" {
            self.prepare_permanent_deletion()?;
        }

        self.persist()?;
        self.invalidate_trash_undo(&affected_ids);

        if action == "delete" {
            self.finish_permanent_deletion()?;
        }

        Ok(Value::Null)
    }

    pub fn public_state(&self) -> Value {
        let items = self
            .data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let can_undo_trash = available_trash_undo(&items, &self.trash_undo_stack);
        let mapped_items: Vec<Value> = items
            .into_iter()
            .map(|mut item| {
                let obj = item.as_object_mut().unwrap();
                let image_file = obj
                    .remove("imageFile")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                let rendered_file = obj
                    .remove("renderedFile")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                let thumbnail_file = obj
                    .remove("thumbnailFile")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                let video_file = obj
                    .remove("videoFile")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                let rendered_video_file = obj
                    .remove("renderedVideoFile")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();

                let id = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let kind = obj
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_rec = obj
                    .get("recording")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let rev_source = if !thumbnail_file.is_empty() {
                    &thumbnail_file
                } else if !rendered_file.is_empty() {
                    &rendered_file
                } else {
                    &image_file
                };
                if rev_source.len() >= 64 {
                    obj.insert("mediaRevision".to_string(), json!(&rev_source[..64]));
                }
                if thumbnail_file.len() >= 64 {
                    obj.insert(
                        "thumbnailUrl".to_string(),
                        json!(format!(
                            "appmedia://media/{}/thumbnail?v={}",
                            id,
                            &thumbnail_file[..64]
                        )),
                    );
                }
                if kind == "image" {
                    let img_src = if !rendered_file.is_empty() {
                        &rendered_file
                    } else {
                        &image_file
                    };
                    let v_hash = if img_src.len() >= 64 {
                        &img_src[..64]
                    } else {
                        img_src.as_str()
                    };
                    obj.insert(
                        "mediaUrl".to_string(),
                        json!(format!("appmedia://media/{}?v={}", id, v_hash)),
                    );
                    obj.insert(
                        "originalMediaUrl".to_string(),
                        json!(format!("appmedia://media/{}/original", id)),
                    );
                }
                if is_rec {
                    let vid_src = if !rendered_video_file.is_empty() {
                        &rendered_video_file
                    } else {
                        &video_file
                    };
                    let v_hash = if vid_src.len() >= 64 {
                        &vid_src[..64]
                    } else {
                        vid_src.as_str()
                    };
                    obj.insert(
                        "videoUrl".to_string(),
                        json!(format!("appmedia://media/{}/video?v={}", id, v_hash)),
                    );
                    obj.insert(
                        "originalVideoUrl".to_string(),
                        json!(format!("appmedia://media/{}/video/original", id)),
                    );
                    obj.insert("sourceFingerprint".to_string(), json!(v_hash));
                }
                item
            })
            .collect();

        let folders = self
            .data
            .get("folders")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let settings = self
            .data
            .get("settings")
            .cloned()
            .unwrap_or_else(default_settings);
        let pending_backup = self
            .data
            .get("pendingBackupRefresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let pending_files = self
            .data
            .get("pendingDeletionFiles")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        json!({
            "items": mapped_items,
            "folders": folders,
            "settings": settings,
            "canUndo": !self.undo_stack.is_empty(),
            "canUndoTrash": can_undo_trash,
            "needsDeletionCleanup": pending_backup || pending_files
        })
    }
}
