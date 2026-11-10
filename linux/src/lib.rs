pub mod clipboard_policy;
pub mod export_drag;
pub mod image_search;
pub mod linux_clipboard;
pub mod linux_media;
pub mod linux_session;
pub mod media_protocol;
pub mod model;
pub mod network;
pub mod ocr;
pub mod shelf_motion;
pub mod shortcuts;
pub mod status_tray;

#[cfg(test)]
mod tests;

use crate::clipboard_policy::{read_clipboard_content, repeated_generation, ClipboardPayload, ClipboardReadResult};
use crate::export_drag::{decode_text_file, export_copies, DragFiles};
use crate::image_search::{
    decode_png_to_bgra_384, item_image_key, prune_cache, search_images_with_worker,
    ImageSearchWorker,
};
use crate::linux_clipboard::{
    clipboard_signature, decodable_png, kept_copy, pixel_fingerprint, write_linux_clipboard,
    KeptClipboard, WaylandClipboard, WaylandWatchers, X11Clipboard,
};
use crate::linux_media::{MediaJobs, MAXIMUM_JOB_MS};
use crate::linux_session::{
    launch_command, linux_session, linux_status_value, linux_tools, manual_updates_state,
    map_animation_delay, portal_screenshot, set_autostart, x11_screenshot, xdotool_capture,
    xdotool_paste, xdotool_restore, LinuxPasteTargets, LinuxSessionInfo, LinuxTools, PasteTarget,
    RELEASES_URL,
};
use crate::media_protocol::{build_media_response, media_generation_matches, media_origin};
use crate::model::{ids, plain_url, Store, MAXIMUM_VIDEO_BYTES};
use crate::network::{bounded_fetch, link_metadata};
use crate::ocr::{recognize_with_tesseract, OcrQueue};
use crate::shelf_motion::{request_motion, MotionBridge};
use crate::shortcuts::{GlobalShortcuts, ShortcutRegistry};
use crate::status_tray::{render_tray_rgba, status_summary, translated};

use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{
    http::{Response as HttpResponse, StatusCode},
    image::Image as TauriImage,
    menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use url::Url;
use uuid::Uuid;

pub const APP_VERSION: &str = "1.7.8";
pub const LINUX_BUILD_NUMBER: u64 = 1;
const BASE_TRAY_ICON_PNG: &[u8] = include_bytes!("../icons/128x128.png");

const BRIDGE_INIT_SCRIPT: &str = r#"
(function () {
  const tauri = window.__TAURI__;
  if (!tauri || !tauri.core || !tauri.event) return;
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;
  try { delete window.__TAURI__; } catch (_) {}
  const bridge = Object.freeze({
    diagnostics: () => ({ sandboxed: true, contextIsolated: true }),
    state: () => invoke('clipboardline_state'),
    command: (action, payload = {}) => invoke('clipboardline_command', { action, payload }),
    subscribeMotion: (listener) => {
      listen('clipboardline:shelf-motion', (event) => {
        const value = event && event.payload;
        if (!value || !Number.isSafeInteger(value.token) || !['prepare', 'open', 'close', 'reset'].includes(value.phase)) return;
        Promise.resolve().then(() => listener(value)).then(
          (result) => invoke('clipboardline_shelf_motion_finished', {
            token: value.token,
            phase: value.phase,
            completed: result ? result.completed !== false : true
          }).catch(() => {}),
          () => invoke('clipboardline_shelf_motion_finished', {
            token: value.token,
            phase: value.phase,
            completed: false
          }).catch(() => {})
        );
      });
    },
    subscribe: (listener) => {
      listen('clipboardline:changed', (event) => {
        if (event && event.payload) listener(event.payload);
      });
    }
  });
  Object.defineProperty(window, 'clipboardLine', {
    value: bridge,
    configurable: false,
    writable: false,
    enumerable: true
  });
})();
"#;

fn media_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^/([0-9a-f-]{36})(/original|/thumbnail|/video(?:/original)?)?$").unwrap()
    })
}

fn recorder_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9:-]{1,100}$").unwrap())
}

pub struct TauriShortcutAdapter {
    pub app: AppHandle,
}

impl ShortcutRegistry for TauriShortcutAdapter {
    fn register(&mut self, key: &str, action: &str) -> bool {
        let action_owned = action.to_string();
        let app_handle = self.app.clone();
        self.app
            .global_shortcut()
            .on_shortcut(key, move |_app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    let app_clone = app_handle.clone();
                    let act = action_owned.clone();
                    tauri::async_runtime::spawn(async move {
                        handle_global_action(&app_clone, &act).await;
                    });
                }
            })
            .is_ok()
    }

    fn unregister(&mut self, key: &str) {
        let _ = self.app.global_shortcut().unregister(key);
    }
}

pub struct AppRuntimeState {
    pub store: Store,
    pub session: LinuxSessionInfo,
    pub tools: LinuxTools,
    pub env_map: HashMap<String, String>,
    pub reveal_delay: u64,
    pub view: String,
    pub view_event: Option<Value>,
    pub message: String,
    pub desired_visible: bool,
    pub visibility_generation: u64,
    pub paste_generation: u64,
    pub last_broadcast_revision: Option<u64>,
    pub clipboard_monitor_mode: String,
    pub clipboard_problem: String,
    pub last_sequence: Option<u64>,
    pub current_sequence: u64,
    pub last_fingerprint: String,
    pub last_ids: Vec<String>,
    pub own_fingerprint: String,
    pub kept_clipboard: Option<KeptClipboard>,
    pub own_writes: usize,
    pub capturing: bool,
    pub pending_clipboard: bool,
    pub paste_targets: LinuxPasteTargets,
    pub previous_target: Option<PasteTarget>,
    pub xdotool_children: Arc<Mutex<Vec<u32>>>,
    pub copy_marker: [u8; 16],
    pub ocr_queue: OcrQueue,
    pub active_ocr: HashSet<String>,
    pub enrich_generations: HashMap<String, u64>,
    pub next_enrich_gen: u64,
    pub recording_trims: HashMap<String, Arc<AtomicBool>>,
    pub shortcut_recording: Option<(String, u64)>,
    pub drag_files: Option<DragFiles>,
    pub media_jobs: Arc<MediaJobs>,
    pub image_search_worker: Arc<Mutex<ImageSearchWorker>>,
    pub image_search_state: Value,
    pub motion_bridge: Arc<Mutex<MotionBridge>>,
    pub shortcuts: Option<GlobalShortcuts<TauriShortcutAdapter>>,
    pub wayland_watchers: Option<Arc<WaylandWatchers>>,
    pub last_tray_key: String,
    pub last_tray_icon_key: String,
    pub last_tray_menu_key: String,
    pub test_mode: bool,
    pub development: bool,
}

pub type SharedState = Arc<Mutex<AppRuntimeState>>;

fn build_state_packet(
    st: &mut AppRuntimeState,
    window: Option<&WebviewWindow>,
    surface_only: bool,
    result: Option<Value>,
) -> Value {
    let light = surface_only && st.last_broadcast_revision == Some(st.store.revision);
    let mut base = if light {
        json!({ "surfaceOnly": true })
    } else {
        st.store.public_state()
    };
    let obj = base.as_object_mut().unwrap();
    obj.insert("libraryRevision".to_string(), json!(st.store.revision));
    obj.insert("version".to_string(), json!(APP_VERSION));
    obj.insert("build".to_string(), json!(LINUX_BUILD_NUMBER));
    obj.insert("platform".to_string(), json!("linux"));
    obj.insert("development".to_string(), json!(st.development));
    obj.insert("view".to_string(), json!(st.view));
    obj.insert(
        "windowMode".to_string(),
        json!(if st.view == "shelves" {
            "overlay"
        } else {
            "normal"
        }),
    );
    let is_max = window
        .and_then(|w| w.is_maximized().ok())
        .unwrap_or(false);
    let is_vis = window.and_then(|w| w.is_visible().ok()).unwrap_or(false);
    obj.insert("windowMaximized".to_string(), json!(is_max));
    obj.insert(
        "windowVisible".to_string(),
        json!(st.desired_visible && is_vis),
    );
    obj.insert(
        "shortcutRecordingToken".to_string(),
        json!(st.shortcut_recording.as_ref().map(|(t, _)| t.clone())),
    );
    let msg = if !st.message.is_empty() {
        st.message.clone()
    } else {
        st.store.privacy_warning.clone()
    };
    obj.insert("message".to_string(), json!(msg));
    obj.insert(
        "viewEvent".to_string(),
        st.view_event.clone().unwrap_or(Value::Null),
    );
    obj.insert("imageSearch".to_string(), st.image_search_state.clone());
    obj.insert(
        "update".to_string(),
        manual_updates_state(APP_VERSION, LINUX_BUILD_NUMBER),
    );
    obj.insert(
        "linux".to_string(),
        linux_status_value(
            &st.session,
            &st.tools,
            &st.clipboard_monitor_mode,
            &st.clipboard_problem,
        ),
    );
    if let Some(res) = result {
        obj.insert("result".to_string(), res);
    }
    base
}

fn broadcast_state(app: &AppHandle, surface_only: bool, result: Option<Value>) -> Value {
    let state_arc = app.state::<SharedState>();
    let window = app.get_webview_window("main");
    let packet = {
        let mut st = state_arc.lock().unwrap();
        let pkt = build_state_packet(&mut st, window.as_ref(), surface_only, result);
        if pkt.get("surfaceOnly").and_then(|v| v.as_bool()) != Some(true) {
            st.last_broadcast_revision = Some(st.store.revision);
        }
        pkt
    };
    if packet.get("surfaceOnly").and_then(|v| v.as_bool()) != Some(true) {
        refresh_tray(app);
    }
    let _ = app.emit("clipboardline:changed", packet.clone());
    packet
}

fn notify_message(app: &AppHandle, text: impl Into<String>) {
    {
        let state_arc = app.state::<SharedState>();
        let mut st = state_arc.lock().unwrap();
        st.message = text.into();
    }
    broadcast_state(app, false, None);
}

fn refresh_tray(app: &AppHandle) {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };
    let state_arc = app.state::<SharedState>();
    let (summary, icon_changed, menu_changed) = {
        let mut st = state_arc.lock().unwrap();
        let items = st
            .store
            .data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let settings = st
            .store
            .data
            .get("settings")
            .cloned()
            .unwrap_or_else(crate::model::default_settings);
        let sys_locale = st.env_map.get("LANG").map(|s| s.as_str());
        let sum = status_summary(&items, &settings, None, sys_locale);
        if sum.key == st.last_tray_key {
            return;
        }
        let ic = sum.icon_key != st.last_tray_icon_key;
        let mc = sum.menu_key != st.last_tray_menu_key;
        st.last_tray_key = sum.key.clone();
        st.last_tray_icon_key = sum.icon_key.clone();
        st.last_tray_menu_key = sum.menu_key.clone();
        (sum, ic, mc)
    };

    let _ = tray.set_tooltip(Some(&summary.tooltip));
    if icon_changed {
        if let Ok((rgba, w, h)) = render_tray_rgba(BASE_TRAY_ICON_PNG, &summary) {
            let _ = tray.set_icon(Some(TauriImage::new_owned(rgba, w, h)));
        }
    }
    if menu_changed {
        let lang = &summary.language;
        if let Ok(menu) = build_tray_menu(app, lang) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn build_tray_menu(
    app: &AppHandle,
    lang: &str,
) -> Result<tauri::menu::Menu<tauri::Wry>, tauri::Error> {
    let show_item =
        MenuItemBuilder::with_id("tray-show", translated(lang, "Show line", &[])).build(app)?;
    let lib_item =
        MenuItemBuilder::with_id("tray-library", translated(lang, "Library", &[])).build(app)?;
    let cap_item =
        MenuItemBuilder::with_id("tray-capture", translated(lang, "Screenshots", &[]))
            .build(app)?;
    let set_item =
        MenuItemBuilder::with_id("tray-settings", translated(lang, "Settings", &[])).build(app)?;
    let chk_item = MenuItemBuilder::with_id(
        "tray-check",
        translated(lang, "Check for updates…", &[]),
    )
    .build(app)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItemBuilder::with_id(
        "tray-quit",
        translated(lang, "Quit %@", &["ClipBoardLine".to_string()]),
    )
    .build(app)?;

    MenuBuilder::new(app)
        .items(&[
            &show_item,
            &lib_item,
            &cap_item,
            &set_item,
            &chk_item,
            &sep,
            &quit_item,
        ])
        .build()
}

pub async fn show_view(app: &AppHandle, next: &str, event: Option<Value>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let state_arc = app.state::<SharedState>();
    let (gen, motion_bridge, wayland_line, reveal_delay, xdotool, own_pid, xdotool_children) = {
        let mut st = state_arc.lock().unwrap();
        for flag in st.recording_trims.values() {
            flag.store(true, Ordering::SeqCst);
        }
        st.visibility_generation += 1;
        st.desired_visible = true;
        st.motion_bridge.lock().unwrap().cancel();
        let xdt = if st.session.backend == "x11" {
            st.tools.xdotool.clone()
        } else {
            None
        };
        (
            st.visibility_generation,
            st.motion_bridge.clone(),
            st.session.backend == "wayland",
            st.reveal_delay,
            xdt,
            st.paste_targets.own_pid,
            st.xdotool_children.clone(),
        )
    };

    let is_vis = window.is_visible().unwrap_or(false);
    let is_foc = window.is_focused().unwrap_or(false);
    if !is_vis || !is_foc {
        let xdt_clone = xdotool.clone();
        let ch_clone = xdotool_children.clone();
        let captured = tokio::task::spawn_blocking(move || {
            xdotool_capture(xdt_clone.as_deref(), own_pid, Some(&ch_clone))
        })
        .await
        .ok()
        .flatten();
        let mut st = state_arc.lock().unwrap();
        if st.visibility_generation != gen {
            return;
        }
        st.previous_target = st.paste_targets.capture(captured);
    }

    let line_full_screen = {
        let mut st = state_arc.lock().unwrap();
        if st.visibility_generation != gen {
            return;
        }
        st.view = next.to_string();
        st.view_event = event.map(|mut ev| {
            if let Some(obj) = ev.as_object_mut() {
                obj.insert("serial".to_string(), json!(Uuid::new_v4().to_string()));
            }
            ev
        });
        st.message.clear();
        st.store
            .data
            .get("settings")
            .and_then(|s| s.get("lineFullScreen"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };

    let is_shelves = next == "shelves";
    let fill = is_shelves && line_full_screen;
    if !fill {
        let _ = window.set_fullscreen(false);
    }
    let _ = window.set_always_on_top(is_shelves);
    let _ = window.set_skip_taskbar(is_shelves);
    let _ = window.set_resizable(!is_shelves || wayland_line);

    if is_shelves {
        if fill {
            let _ = window.set_fullscreen(true);
        } else if wayland_line {
            let _ = window.maximize();
        } else if let Ok(Some(monitor)) = window.current_monitor().or_else(|_| window.primary_monitor()) {
            let pos = monitor.position();
            let size = monitor.size();
            let scale = monitor.scale_factor();
            let _ = window.set_position(LogicalPosition::new(
                (pos.x as f64) / scale,
                (pos.y as f64) / scale,
            ));
            let _ = window.set_size(LogicalSize::new(
                (size.width as f64) / scale,
                (size.height as f64) / scale,
            ));
        }
    } else {
        let _ = window.set_fullscreen(false);
        let _ = window.unmaximize();
        let _ = window.set_size(LogicalSize::new(1180.0, 800.0));
        let _ = window.center();
    }

    let mapping = !is_vis;
    let animate = is_shelves && (mapping || !is_foc);
    let warm = animate && mapping && reveal_delay > 0;
    let app_for_motion = app.clone();
    if animate {
        request_motion(
            &motion_bridge,
            "prepare",
            json!({
                "hidden": !is_vis || !is_foc,
                "warm": warm
            }),
            |payload| app_for_motion.emit("clipboardline:shelf-motion", payload).map_err(|e| e.to_string()),
        )
        .await;
    } else {
        let app_reset = app.clone();
        request_motion(&motion_bridge, "reset", json!({}), |payload| {
            app_reset.emit("clipboardline:shelf-motion", payload).map_err(|e| e.to_string())
        })
        .await;
    }

    {
        let st = state_arc.lock().unwrap();
        if st.visibility_generation != gen {
            return;
        }
    }

    let _ = window.show();
    let _ = window.set_focus();
    broadcast_state(app, true, None);

    if animate {
        let app_open = app.clone();
        request_motion(
            &motion_bridge,
            "open",
            if warm {
                json!({ "after": reveal_delay })
            } else {
                json!({})
            },
            |payload| app_open.emit("clipboardline:shelf-motion", payload).map_err(|e| e.to_string()),
        )
        .await;
    }
}

pub async fn hide_view(app: &AppHandle, restore: bool) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let state_arc = app.state::<SharedState>();
    let (gen, view_now, motion_bridge, target, xdotool, own_pid, xdotool_children) = {
        let mut st = state_arc.lock().unwrap();
        for flag in st.recording_trims.values() {
            flag.store(true, Ordering::SeqCst);
        }
        st.visibility_generation += 1;
        st.desired_visible = false;
        st.motion_bridge.lock().unwrap().cancel();
        let target = st.previous_target.take();
        st.view_event = None;
        let xdt = if st.session.backend == "x11" {
            st.tools.xdotool.clone()
        } else {
            None
        };
        (
            st.visibility_generation,
            st.view.clone(),
            st.motion_bridge.clone(),
            target,
            xdt,
            st.paste_targets.own_pid,
            st.xdotool_children.clone(),
        )
    };

    broadcast_state(app, true, None);
    if view_now == "shelves" && window.is_visible().unwrap_or(false) {
        let app_close = app.clone();
        request_motion(&motion_bridge, "close", json!({}), |payload| {
            app_close.emit("clipboardline:shelf-motion", payload).map_err(|e| e.to_string())
        })
        .await;
    }

    {
        let st = state_arc.lock().unwrap();
        if st.visibility_generation != gen {
            return;
        }
    }
    let _ = window.hide();
    if restore && target.is_some() {
        let _ = tokio::task::spawn_blocking(move || {
            xdotool_restore(xdotool.as_deref(), target.as_ref(), own_pid, Some(&xdotool_children));
        })
        .await;
    }
}

async fn handle_global_action(app: &AppHandle, action: &str) {
    let state_arc = app.state::<SharedState>();
    match action {
        "toggle" => {
            let vis = state_arc.lock().unwrap().desired_visible;
            if vis {
                hide_view(app, true).await;
            } else {
                show_view(app, "shelves", None).await;
            }
        }
        "search" => {
            show_view(app, "shelves", Some(json!({ "search": true }))).await;
        }
        "library" => {
            show_view(app, "library", None).await;
        }
        "copyLatest" => {
            let latest_img = {
                let st = state_arc.lock().unwrap();
                let items = st
                    .store
                    .data
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut imgs: Vec<Value> = items
                    .into_iter()
                    .filter(|i| {
                        i.get("kind").and_then(|v| v.as_str()) == Some("image")
                            && i.get("trashedAt").map(|v| v.is_null()).unwrap_or(true)
                            && !i
                                .get("hiddenFromShelf")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                    })
                    .collect();
                imgs.sort_by(|a, b| {
                    let cb = b.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
                    let ca = a.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
                    cb.cmp(&ca)
                });
                imgs.first().cloned()
            };
            if let Some(img) = latest_img {
                if write_selected_to_clipboard(app, &[img]).await.is_ok() {
                    notify_message(app, "Copied");
                }
            }
        }
        "screenshot" => {
            let _ = capture_screenshot_action(app).await;
        }
        _ => {}
    }
}

fn read_current_linux_clipboard(
    session: &LinuxSessionInfo,
    tools: &LinuxTools,
    seq: u64,
    marker: &[u8],
    capture_links: bool,
) -> ClipboardReadResult {
    let mut res = if session.backend == "wayland" {
        let Some(ref wl_paste) = tools.wl_paste else {
            return ClipboardReadResult::Empty;
        };
        let reader = WaylandClipboard::new(wl_paste);
        read_clipboard_content(&reader, || Ok(seq), marker, capture_links)
    } else {
        let Some(ref xclip) = tools.xclip else {
            return ClipboardReadResult::Empty;
        };
        let reader = X11Clipboard::new(xclip);
        read_clipboard_content(&reader, || Ok(seq), marker, capture_links)
    };
    if let ClipboardReadResult::Content(ref mut content) = res {
        if let ClipboardPayload::Image(ref bytes) = content.payload {
            if let Some(pf) = pixel_fingerprint(bytes) {
                content.fingerprint = pf;
            }
        }
    }
    res
}

fn is_linux_clipboard_empty(session: &LinuxSessionInfo, tools: &LinuxTools) -> bool {
    if session.backend == "wayland" {
        let Some(ref wl_paste) = tools.wl_paste else {
            return false;
        };
        WaylandClipboard::new(wl_paste)
            .types()
            .map(|t| t.is_empty())
            .unwrap_or(false)
    } else {
        let Some(ref xclip) = tools.xclip else {
            return false;
        };
        X11Clipboard::new(xclip)
            .types()
            .map(|t| t.is_empty())
            .unwrap_or(false)
    }
}

pub async fn poll_and_capture_clipboard(app: &AppHandle) {
    loop {
        let state_arc = app.state::<SharedState>();
        let (session, tools, seq, marker, capture_links, any_enabled) = {
        let mut st = state_arc.lock().unwrap();
        if st.capturing || st.own_writes > 0 {
            st.pending_clipboard = true;
            return;
        }
        let settings = st.store.data.get("settings").cloned().unwrap_or_default();
        let cap_text = settings
            .get("captureText")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let cap_img = settings
            .get("captureImages")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let cap_link = settings
            .get("captureLinks")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !cap_text && !cap_img && !cap_link {
            return;
        }
        st.capturing = true;
        (
            st.session.clone(),
            st.tools.clone(),
            st.current_sequence,
            st.copy_marker,
            cap_link,
            true,
        )
    };
    if !any_enabled {
        return;
    }

    let sess_clone = session.clone();
    let tools_clone = tools.clone();
    let read_res = tokio::task::spawn_blocking(move || {
        read_current_linux_clipboard(&sess_clone, &tools_clone, seq, &marker, capture_links)
    })
    .await
    .unwrap_or(ClipboardReadResult::Empty);

    let mut post_item: Option<Value> = None;
    let mut should_show_after_screenshot = false;

    match read_res {
        ClipboardReadResult::Empty => {
            let kept_opt = {
                let mut st = state_arc.lock().unwrap();
                let items = st
                    .store
                    .data
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let k = kept_copy(st.kept_clipboard.as_ref(), &st.last_fingerprint, &items).cloned();
                st.kept_clipboard = None;
                k
            };
            if let Some(kept) = kept_opt {
                let sess2 = session.clone();
                let tools2 = tools.clone();
                let is_empty =
                    tokio::task::spawn_blocking(move || is_linux_clipboard_empty(&sess2, &tools2))
                        .await
                        .unwrap_or(false);
                if is_empty {
                    {
                        let mut st = state_arc.lock().unwrap();
                        st.own_writes += 1;
                    }
                    let sess3 = session.clone();
                    let tools3 = tools.clone();
                    let val_bytes = kept.value.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        write_linux_clipboard(
                            "",
                            Some(&val_bytes),
                            &[],
                            sess3.backend == "wayland",
                            tools3.wl_copy.as_deref(),
                            tools3.wl_paste.as_deref(),
                            tools3.xclip.as_deref(),
                        )
                    })
                    .await;
                    let mut st = state_arc.lock().unwrap();
                    st.own_fingerprint = kept.fingerprint;
                    st.own_writes = st.own_writes.saturating_sub(1);
                }
            }
        }
        ClipboardReadResult::Raced => {
            let mut st = state_arc.lock().unwrap();
            st.pending_clipboard = true;
        }
        ClipboardReadResult::Content(content) => {
            let mut st = state_arc.lock().unwrap();
            if st.own_writes > 0 {
                st.pending_clipboard = true;
            } else if !repeated_generation(&mut st.last_sequence, content.sequence) {
                let items = st
                    .store
                    .data
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let on_shelf = st.last_ids.iter().any(|id| {
                    items.iter().any(|i| {
                        i.get("id").and_then(|v| v.as_str()) == Some(id.as_str())
                            && i.get("trashedAt").map(|v| v.is_null()).unwrap_or(true)
                            && !i
                                .get("hiddenFromShelf")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                    })
                });
                let repeat = content.kind == "image"
                    && content.fingerprint == st.last_fingerprint
                    && on_shelf;
                st.last_fingerprint = content.fingerprint.clone();
                if !repeat {
                    st.kept_clipboard = None;
                    st.last_ids.clear();
                    if content.fingerprint == st.own_fingerprint {
                        st.own_fingerprint.clear();
                    } else if !content.owned {
                        let setting_key = match content.kind.as_str() {
                            "image" => "captureImages",
                            "text" => "captureText",
                            "link" => "captureLinks",
                            _ => "",
                        };
                        let enabled = st
                            .store
                            .data
                            .get("settings")
                            .and_then(|s| s.get(setting_key))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if enabled {
                            let inserted = match &content.payload {
                                ClipboardPayload::Image(bytes) => st.store.insert_image(bytes, None),
                                ClipboardPayload::Link(url) => st.store.insert_link(url, None),
                                ClipboardPayload::Text(txt) => {
                                    st.store.insert_text(txt, None, false, false)
                                }
                            };
                            if let Ok(item) = inserted {
                                let id = item
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if let ClipboardPayload::Image(ref bytes) = content.payload {
                                    st.kept_clipboard = Some(KeptClipboard {
                                        id: id.clone(),
                                        kind: "image".to_string(),
                                        value: bytes.clone(),
                                        fingerprint: content.fingerprint.clone(),
                                    });
                                }
                                st.last_ids = vec![id];
                                let open_after = st
                                    .store
                                    .data
                                    .get("settings")
                                    .and_then(|s| s.get("openAfterScreenshot"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(true);
                                should_show_after_screenshot =
                                    content.kind == "image" && open_after && !st.desired_visible;
                                post_item = Some(item);
                            }
                        }
                    }
                }
            }
        }
    }

    let retry = {
        let mut st = state_arc.lock().unwrap();
        st.capturing = false;
        if st.pending_clipboard && st.own_writes == 0 {
            st.pending_clipboard = false;
            true
        } else {
            false
        }
    };

    if let Some(item) = post_item {
        broadcast_state(app, false, None);
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kind = item
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if kind == "link" {
            let app_clone = app.clone();
            let id_clone = id.clone();
            tauri::async_runtime::spawn(async move {
                let _ = enrich_link(&app_clone, &id_clone, false).await;
            });
        } else if kind == "image" {
            enqueue_ocr_and_drain(app, &id);
            trigger_image_search_refresh(app);
            if should_show_after_screenshot {
                show_view(
                    app,
                    "shelves",
                    Some(json!({ "selectId": id, "kind": "image" })),
                )
                .await;
            }
        }
    }

    if !retry {
        break;
    }
    }
}

async fn write_selected_to_clipboard(app: &AppHandle, selected: &[Value]) -> Result<(), String> {
    if selected.is_empty() {
        return Err("Select an item first".to_string());
    }
    let state_arc = app.state::<SharedState>();
    let (text_joined, png_bytes, files, session, tools, marker, selected_ids) = {
        let mut st = state_arc.lock().unwrap();
        let images: Vec<&Value> = selected
            .iter()
            .filter(|i| i.get("kind").and_then(|v| v.as_str()) == Some("image"))
            .collect();
        let values: Vec<String> = selected
            .iter()
            .filter(|i| i.get("kind").and_then(|v| v.as_str()) != Some("image"))
            .map(|i| {
                if i.get("kind").and_then(|v| v.as_str()) == Some("link") {
                    i.get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    i.get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                }
            })
            .collect();

        let mut files = Vec::new();
        if images.len() > 1
            || images.iter().any(|i| {
                i.get("recording")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
        {
            for img in &images {
                let id = img.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let mp = st.store.media_path(id, false).map_err(|e| e.message)?;
                files.push(mp);
            }
        }

        let mut png_bytes = None;
        if let Some(still) = images.iter().find(|i| {
            !i.get("recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        }) {
            let id = still.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let mp = st.store.media_path(id, false).map_err(|e| e.message)?;
            let bytes = fs::read(&mp).map_err(|_| "Image unavailable".to_string())?;
            if !decodable_png(&bytes) {
                return Err("Image unavailable".to_string());
            }
            png_bytes = Some(bytes);
        }

        st.own_writes += 1;
        let sel_ids: Vec<String> = selected
            .iter()
            .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        (
            values.join("\n\n"),
            png_bytes,
            files,
            st.session.clone(),
            st.tools.clone(),
            st.copy_marker,
            sel_ids,
        )
    };

    let sess_w = session.clone();
    let tools_w = tools.clone();
    let write_res = tokio::task::spawn_blocking(move || {
        write_linux_clipboard(
            &text_joined,
            png_bytes.as_deref(),
            &files,
            sess_w.backend == "wayland",
            tools_w.wl_copy.as_deref(),
            tools_w.wl_paste.as_deref(),
            tools_w.xclip.as_deref(),
        )?;
        let read_back = read_current_linux_clipboard(&sess_w, &tools_w, 1, &marker, true);
        let fp = match read_back {
            ClipboardReadResult::Content(c) => c.fingerprint,
            _ => String::new(),
        };
        Ok::<String, String>(fp)
    })
    .await
    .map_err(|e| e.to_string())?;

    {
        let mut st = state_arc.lock().unwrap();
        st.own_writes = st.own_writes.saturating_sub(1);
        if let Ok(ref fp) = write_res {
            st.own_fingerprint = fp.clone();
            st.last_fingerprint = fp.clone();
            st.last_ids = selected_ids.clone();
            if !selected_ids.is_empty() {
                let _ = st.store.modify("markCopied", &json!({ "ids": selected_ids }));
            }
        }
    }

    write_res.map(|_| ())
}

async fn enrich_link(app: &AppHandle, id: &str, refresh: bool) -> Result<(), String> {
    let state_arc = app.state::<SharedState>();
    let (gen, url) = {
        let mut st = state_arc.lock().unwrap();
        let fetch_enabled = st
            .store
            .data
            .get("settings")
            .and_then(|s| s.get("fetchLinkMetadata"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !fetch_enabled || (!refresh && st.enrich_generations.contains_key(id)) {
            return Ok(());
        }
        let item = st.store.item(id).map_err(|e| e.message)?;
        if item.get("kind").and_then(|v| v.as_str()) != Some("link") {
            return Ok(());
        }
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        st.next_enrich_gen += 1;
        let gen = st.next_enrich_gen;
        st.enrich_generations.insert(id.to_string(), gen);

        if refresh {
            let host = Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_string()))
                .unwrap_or_default();
            if let Some(items) = st.store.data.get_mut("items").and_then(|v| v.as_array_mut()) {
                if let Some(it) = items
                    .iter_mut()
                    .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(id))
                {
                    let obj = it.as_object_mut().unwrap();
                    obj.remove("thumbnailFile");
                    obj.remove("author");
                    obj.insert("title".to_string(), json!(host));
                }
            }
            let _ = st.store.persist();
        }
        (gen, url)
    };

    if refresh {
        broadcast_state(app, false, None);
    }

    let meta_res = link_metadata(&url).await;
    let is_current = |st: &AppRuntimeState| -> bool {
        let fetch_enabled = st
            .store
            .data
            .get("settings")
            .and_then(|s| s.get("fetchLinkMetadata"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        fetch_enabled && st.enrich_generations.get(id) == Some(&gen)
    };

    match meta_res {
        Ok(metadata) => {
            let mut thumb_png: Option<Vec<u8>> = None;
            if !metadata.thumbnail.is_empty() {
                if let Ok(resp) =
                    bounded_fetch(&metadata.thumbnail, 2 * 1024 * 1024, 3, 8000, 20000, true).await
                {
                    if let Ok(img) = image::load_from_memory(&resp.bytes) {
                        let (w, h) = (img.width(), img.height());
                        if (w as u64) * (h as u64) <= 16_000_000 {
                            let target_w = w.min(640);
                            let target_h = ((h as u64 * target_w as u64) / w.max(1) as u64).max(1) as u32;
                            let resized = if target_w < w {
                                img.resize_exact(
                                    target_w,
                                    target_h,
                                    image::imageops::FilterType::Triangle,
                                )
                            } else {
                                img
                            };
                            let mut buf = Vec::new();
                            if resized
                                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                                .is_ok()
                            {
                                thumb_png = Some(buf);
                            }
                        }
                    }
                }
            }
            let updated = {
                let mut st = state_arc.lock().unwrap();
                let cur = is_current(&st);
                if st.enrich_generations.get(id) == Some(&gen) {
                    st.enrich_generations.remove(id);
                }
                if cur {
                    st.store.set_link_metadata(
                        id,
                        &url,
                        Some(&metadata.title),
                        Some(&metadata.author),
                        thumb_png.as_deref(),
                    )
                } else {
                    false
                }
            };
            if updated {
                broadcast_state(app, false, None);
            }
            Ok(())
        }
        Err(e) => {
            let cur = {
                let mut st = state_arc.lock().unwrap();
                let c = is_current(&st);
                if st.enrich_generations.get(id) == Some(&gen) {
                    st.enrich_generations.remove(id);
                }
                c
            };
            if refresh && cur {
                Err(format!("Link preview could not be refreshed: {}", e))
            } else {
                Ok(())
            }
        }
    }
}

fn enqueue_ocr_and_drain(app: &AppHandle, id: &str) {
    let state_arc = app.state::<SharedState>();
    let queued = {
        let mut st = state_arc.lock().unwrap();
        let enabled = !st.test_mode
            && st
                .store
                .data
                .get("settings")
                .and_then(|s| s.get("indexImages"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
        st.ocr_queue.enqueue(id, enabled, false)
    };
    if queued {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let next_opt = {
                    let st_arc = app_clone.state::<SharedState>();
                    let mut st = st_arc.lock().unwrap();
                    st.ocr_queue.dequeue()
                };
                let Some(next_id) = next_opt else {
                    break;
                };
                let _ = run_ocr_for_item(&app_clone, &next_id, true).await;
            }
        });
    }
}

async fn run_ocr_for_item(app: &AppHandle, id: &str, quiet: bool) -> Result<Value, String> {
    let state_arc = app.state::<SharedState>();
    let (file_path, expected_fp, tesseract_bin) = {
        let st = state_arc.lock().unwrap();
        let item = st.store.item(id).map_err(|e| e.message)?;
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let is_rec = item
            .get("recording")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if kind != "image" || is_rec {
            return Err("OCR requires a still image".to_string());
        }
        let existing_ocr = item.get("ocrText").and_then(|v| v.as_str()).unwrap_or("");
        if !existing_ocr.is_empty() {
            return Ok(json!({ "text": existing_ocr, "id": id }));
        }
        let file = st.store.media_path(id, false).map_err(|e| e.message)?;
        let fp = item
            .get("renderedFile")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| item.get("imageFile").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        (file, fp, st.tools.tesseract.clone())
    };

    if !quiet {
        notify_message(app, "Recognizing image text locally…");
    }

    let id_owned = id.to_string();
    let recognized = tokio::task::spawn_blocking(move || {
        recognize_with_tesseract(tesseract_bin.as_deref(), &file_path)
    })
    .await
    .map_err(|e| e.to_string())??;

    let updated = {
        let mut st = state_arc.lock().unwrap();
        st.store.set_ocr_text(&id_owned, &expected_fp, &recognized)
    };
    if !updated {
        return Err("Image changed during recognition".to_string());
    }
    if !quiet {
        notify_message(
            app,
            if !recognized.is_empty() {
                "Image text ready"
            } else {
                "No text found"
            },
        );
    } else {
        broadcast_state(app, false, None);
    }
    Ok(json!({ "text": recognized, "id": id }))
}

fn trigger_image_search_refresh(app: &AppHandle) {
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let state_arc = app_clone.state::<SharedState>();
        let (worker, cache_dir, jobs) = {
            let st = state_arc.lock().unwrap();
            let worker = st.image_search_worker.clone();
            let (avail, cache_dir) = {
                let w = worker.lock().unwrap();
                (w.available, w.cache_directory.clone())
            };
            if !avail {
                return;
            }
            let items = st
                .store
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut jobs = Vec::new();
            for item in &items {
                if let (Some(id), Some(key)) = (
                    item.get("id").and_then(|v| v.as_str()),
                    item_image_key(item),
                ) {
                    if let Ok(poster) = st.store.poster_path(id, false) {
                        jobs.push((id.to_string(), key, poster));
                    }
                }
            }
            (worker, cache_dir, jobs)
        };

        let live_keys: Vec<String> = jobs.iter().map(|(_, k, _)| k.clone()).collect();
        let _ = prune_cache(&cache_dir, &live_keys);

        for (_id, key, poster) in jobs {
            let already = worker.lock().unwrap().has(&key).unwrap_or(false);
            if already {
                continue;
            }
            let poster_clone = poster.clone();
            let decoded = tokio::task::spawn_blocking(move || decode_png_to_bgra_384(&poster_clone))
                .await
                .ok()
                .and_then(|r| r.ok())
                .flatten();
            if let Some((bgra, w, h)) = decoded {
                let worker_clone = worker.clone();
                let key_clone = key.clone();
                let indexed = tokio::task::spawn_blocking(move || {
                    worker_clone.lock().unwrap().index(&key_clone, &bgra, w, h)
                })
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(false);
                if indexed {
                    {
                        let mut st = state_arc.lock().unwrap();
                        let rev = st
                            .image_search_state
                            .get("revision")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                            + 1;
                        st.image_search_state = json!({
                            "available": true,
                            "indexing": false,
                            "pending": 0,
                            "revision": rev
                        });
                    }
                    broadcast_state(&app_clone, false, None);
                }
            }
        }
    });
}

async fn capture_screenshot_action(app: &AppHandle) -> Result<Value, String> {
    let window = app.get_webview_window("main");
    let was_visible = window
        .as_ref()
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let state_arc = app.state::<SharedState>();
    let (session, tools, motion_bridge, prev_view) = {
        let mut st = state_arc.lock().unwrap();
        st.visibility_generation += 1;
        st.desired_visible = false;
        st.motion_bridge.lock().unwrap().cancel();
        st.view_event = None;
        (
            st.session.clone(),
            st.tools.clone(),
            st.motion_bridge.clone(),
            st.view.clone(),
        )
    };
    if was_visible {
        if let Some(ref w) = window {
            let _ = w.hide();
        }
    }
    broadcast_state(app, true, None);
    let app_reset = app.clone();
    request_motion(&motion_bridge, "reset", json!({}), |payload| {
        app_reset
            .emit("clipboardline:shelf-motion", payload)
            .map_err(|e| e.to_string())
    })
    .await;

    tokio::time::sleep(Duration::from_millis(150)).await;

    let display_env = state_arc
        .lock()
        .unwrap()
        .env_map
        .get("DISPLAY")
        .cloned()
        .unwrap_or_else(|| ":0".to_string());
    let monitor_rect = window
        .as_ref()
        .and_then(|w| w.current_monitor().ok().flatten())
        .map(|m| {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width, s.height)
        })
        .unwrap_or((0, 0, 1920, 1080));

    let png_res = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        if session.session == "wayland" {
            let requested = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let file = portal_screenshot(tools.gdbus.as_deref(), 120_000)?;
            let read_res = (|| -> Result<Vec<u8>, String> {
                let bytes =
                    fs::read(&file).map_err(|_| "Screen capture is unavailable".to_string())?;
                if decodable_png(&bytes) {
                    return Ok(bytes);
                }
                let img = image::load_from_memory(&bytes)
                    .map_err(|_| "Screen capture is unavailable".to_string())?;
                let mut out = Vec::new();
                img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
                    .map_err(|_| "Screen capture is unavailable".to_string())?;
                Ok(out)
            })();
            if let Ok(meta) = fs::symlink_metadata(&file) {
                if meta.mtime() >= requested - 2 {
                    let _ = fs::remove_file(&file);
                }
            }
            read_res
        } else {
            x11_screenshot(
                tools.ffmpeg.as_deref(),
                &display_env,
                monitor_rect.0,
                monitor_rect.1,
                monitor_rect.2,
                monitor_rect.3,
            )
            .or_else(|_| {
                let file = portal_screenshot(tools.gdbus.as_deref(), 120_000)?;
                let bytes =
                    fs::read(&file).map_err(|_| "Screen capture is unavailable".to_string())?;
                let _ = fs::remove_file(&file);
                Ok(bytes)
            })
        }
    })
    .await
    .map_err(|e| e.to_string())?;

    match png_res {
        Ok(png_bytes) => {
            let item = {
                let mut st = state_arc.lock().unwrap();
                st.store
                    .insert_image(&png_bytes, None)
                    .map_err(|e| e.message)?
            };
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            enqueue_ocr_and_drain(app, &id);
            trigger_image_search_refresh(app);
            show_view(
                app,
                "shelves",
                Some(json!({
                    "selectId": id,
                    "kind": "image",
                    "preview": true,
                    "crop": true
                })),
            )
            .await;
            Ok(json!({ "id": id }))
        }
        Err(e) => {
            if was_visible {
                show_view(app, &prev_view, None).await;
            }
            Err(e)
        }
    }
}

pub fn import_single_file(app: &AppHandle, file: &Path) -> Result<Value, String> {
    let meta = fs::symlink_metadata(file).map_err(|_| "Select a regular file.".to_string())?;
    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return Err("Select a regular file.".to_string());
    }
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    let state_arc = app.state::<SharedState>();

    if ext == ".mov" || ext == ".mp4" {
        if meta.len() > MAXIMUM_VIDEO_BYTES {
            return Err("Recording exceeds the 2 GB import limit.".to_string());
        }
        let media_jobs = state_arc.lock().unwrap().media_jobs.clone();
        let inspected = media_jobs.execute("Inspect", file, None, None, false, None)?;
        let res = (|| -> Result<Value, String> {
            let after = fs::symlink_metadata(file)
                .map_err(|_| "The recording changed while importing.".to_string())?;
            if after.file_type().is_symlink()
                || after.len() != meta.len()
                || after.mtime() != meta.mtime()
                || after.mtime_nsec() != meta.mtime_nsec()
                || after.ino() != meta.ino()
            {
                return Err("The recording changed while importing.".to_string());
            }
            let item = state_arc
                .lock()
                .unwrap()
                .store
                .insert_recording(&inspected, None)
                .map_err(|e| e.message)?;
            trigger_image_search_refresh(app);
            Ok(item)
        })();
        media_jobs.release(&inspected);
        return res;
    }

    if meta.len() > 64 * 1024 * 1024 {
        return Err("File exceeds import limit".to_string());
    }
    if ext == ".txt" || ext == ".md" {
        if meta.len() > 1024 * 1024 {
            return Err("Text file exceeds import limit".to_string());
        }
        let raw = fs::read(file).map_err(|e| e.to_string())?;
        let text = decode_text_file(&raw)?;
        let mut st = state_arc.lock().unwrap();
        if let Some(url) = plain_url(&text) {
            return st.store.insert_link(&url, None).map_err(|e| e.message);
        } else {
            return st
                .store
                .insert_text(&text, None, false, false)
                .map_err(|e| e.message);
        }
    }

    let raw = fs::read(file).map_err(|_| "Unsupported image file".to_string())?;
    let png_bytes = if decodable_png(&raw) {
        raw
    } else {
        let img =
            image::load_from_memory(&raw).map_err(|_| "Unsupported image file".to_string())?;
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|_| "Unsupported image file".to_string())?;
        out
    };
    let item = state_arc
        .lock()
        .unwrap()
        .store
        .insert_image(&png_bytes, None)
        .map_err(|e| e.message)?;
    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
        enqueue_ocr_and_drain(app, id);
    }
    trigger_image_search_refresh(app);
    Ok(item)
}

fn pick_files_native(directory: bool, multiple: bool) -> Vec<PathBuf> {
    if let Ok(out) = Command::new("zenity")
        .args(if directory {
            vec!["--file-selection", "--directory"]
        } else if multiple {
            vec!["--file-selection", "--multiple", "--separator=\n"]
        } else {
            vec!["--file-selection"]
        })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
        }
        return Vec::new();
    }
    if let Ok(out) = Command::new("kdialog")
        .args(if directory {
            vec!["--getexistingdirectory"]
        } else if multiple {
            vec!["--getopenfilename", ".", "--multiple", "--separate-output"]
        } else {
            vec!["--getopenfilename", "."]
        })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
        }
    }
    Vec::new()
}

fn open_external_url_or_path(target: &str) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn reveal_file_in_folder(file: &Path) {
    if let Ok(file_url) = Url::from_file_path(file) {
        let ok = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.freedesktop.FileManager1",
                "--object-path",
                "/org/freedesktop/FileManager1",
                "--method",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("['{}']", file_url.as_str()),
                "",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
    if let Some(parent) = file.parent() {
        let _ = open_external_url_or_path(&parent.to_string_lossy());
    }
}

#[tauri::command]
async fn clipboardline_state(app: AppHandle) -> Result<Value, String> {
    let state_arc = app.state::<SharedState>();
    let window = app.get_webview_window("main");
    let mut st = state_arc.lock().unwrap();
    Ok(build_state_packet(&mut st, window.as_ref(), false, None))
}

#[tauri::command]
async fn clipboardline_shelf_motion_finished(
    app: AppHandle,
    token: u64,
    phase: String,
    completed: bool,
) -> Result<bool, String> {
    let state_arc = app.state::<SharedState>();
    let bridge = state_arc.lock().unwrap().motion_bridge.clone();
    let ack = bridge.lock().unwrap().acknowledge(token, &phase, completed);
    Ok(ack)
}

#[tauri::command]
async fn clipboardline_command(
    app: AppHandle,
    action: String,
    payload: Option<Value>,
) -> Result<Value, String> {
    let payload = payload.unwrap_or_else(|| json!({}));
    if !payload.is_object() {
        return Err("Invalid command".to_string());
    }
    let payload_len = serde_json::to_string(&payload)
        .map(|s| crate::model::utf16_len(&s))
        .unwrap_or(usize::MAX);
    if payload_len > 92_000_000 {
        return Err("Invalid command".to_string());
    }

    let state_arc = app.state::<SharedState>();
    {
        let mut st = state_arc.lock().unwrap();
        st.message.clear();
    }

    let model_actions = [
        "pin",
        "name",
        "details",
        "retain",
        "note",
        "unassign",
        "removeTag",
        "renameTag",
        "deleteTag",
        "folder",
        "createFolder",
        "renameFolder",
        "removeFolder",
        "tag",
        "remove",
        "undo",
        "trash",
        "trashUndoably",
        "undoTrash",
        "restore",
        "delete",
        "editText",
        "saveImage",
        "cleanup",
        "clearUnsavedHistory",
    ];

    let mut cmd_result: Option<Value> = None;

    if action == "beginShortcutRecording" {
        let Some(token) = payload.get("token").and_then(|v| v.as_str()) else {
            return Err("Invalid shortcut recorder.".to_string());
        };
        if !recorder_token_re().is_match(token) {
            return Err("Invalid shortcut recorder.".to_string());
        }
        let window = app.get_webview_window("main");
        let vis = window
            .as_ref()
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        let foc = window
            .as_ref()
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false);
        if !vis || !foc {
            return Err("Focus the shortcut recorder before recording.".to_string());
        }
        let mut st = state_arc.lock().unwrap();
        if st.shortcut_recording.as_ref().map(|(t, _)| t.as_str()) != Some(token) {
            if let Some((_, old_lease)) = st.shortcut_recording.take() {
                if let Some(ref mut sc) = st.shortcuts {
                    sc.resume(old_lease);
                }
            }
            if let Some(ref mut sc) = st.shortcuts {
                let lease = sc.suspend()?;
                st.shortcut_recording = Some((token.to_string(), lease));
            }
        }
    } else if action == "endShortcutRecording" {
        if let Some(token) = payload.get("token").and_then(|v| v.as_str()) {
            let mut st = state_arc.lock().unwrap();
            if st.shortcut_recording.as_ref().map(|(t, _)| t.as_str()) == Some(token) {
                if let Some((_, lease)) = st.shortcut_recording.take() {
                    if let Some(ref mut sc) = st.shortcuts {
                        sc.resume(lease);
                    }
                }
            }
        }
    } else if model_actions.contains(&action.as_str()) {
        if action == "saveImage" {
            let Some(data_url) = payload.get("dataUrl").and_then(|v| v.as_str()) else {
                return Err("Invalid edited image".to_string());
            };
            if crate::model::utf16_len(data_url) > 90_000_000 {
                return Err("Invalid edited image".to_string());
            }
            let b64_part = data_url.split(',').nth(1).unwrap_or("");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(b64_part)
                .map_err(|_| "Invalid edited image".to_string())?;
            if !decodable_png(&decoded) {
                return Err("Invalid edited image".to_string());
            }
        }
        let res = {
            let mut st = state_arc.lock().unwrap();
            let r = st
                .store
                .modify(&action, &payload)
                .map_err(|e| e.message)?;
            if [
                "remove",
                "trash",
                "trashUndoably",
                "delete",
                "clearUnsavedHistory",
            ]
            .contains(&action.as_str())
            {
                let removed_ids = if ["clearUnsavedHistory", "trash", "trashUndoably"]
                    .contains(&action.as_str())
                {
                    r.get("ids").cloned().unwrap_or_else(|| json!([]))
                } else {
                    payload
                        .get("ids")
                        .cloned()
                        .or_else(|| payload.get("id").map(|id| json!([id])))
                        .unwrap_or_else(|| json!([]))
                };
                st.view_event = Some(json!({
                    "removedIds": removed_ids,
                    "serial": Uuid::new_v4().to_string()
                }));
            }
            if let Some(ref kept) = st.kept_clipboard {
                let items = st
                    .store
                    .data
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let on_shelf = items.iter().any(|i| {
                    i.get("id").and_then(|v| v.as_str()) == Some(kept.id.as_str())
                        && i.get("trashedAt").map(|v| v.is_null()).unwrap_or(true)
                        && !i
                            .get("hiddenFromShelf")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                });
                if !on_shelf {
                    st.kept_clipboard = None;
                }
            }
            r
        };
        if action == "saveImage" {
            if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                enqueue_ocr_and_drain(&app, id);
            }
        }
        if [
            "saveImage",
            "remove",
            "delete",
            "trash",
            "trashUndoably",
            "undoTrash",
            "restore",
            "undo",
            "cleanup",
            "clearUnsavedHistory",
        ]
        .contains(&action.as_str())
        {
            trigger_image_search_refresh(&app);
        }
        if !res.is_null() {
            cmd_result = Some(res);
        }
    } else if action == "show" || action == "search" {
        let ev = if action == "search" {
            Some(json!({ "search": true }))
        } else {
            None
        };
        show_view(&app, "shelves", ev).await;
    } else if action == "hide" || action == "windowClose" {
        hide_view(&app, true).await;
    } else if action == "library" || action == "settings" {
        show_view(&app, &action, None).await;
    } else if action == "windowMinimize" {
        let view_now = state_arc.lock().unwrap().view.clone();
        if view_now != "shelves" {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.minimize();
            }
        }
    } else if action == "windowMaximize" {
        let view_now = state_arc.lock().unwrap().view.clone();
        if view_now != "shelves" {
            if let Some(w) = app.get_webview_window("main") {
                if w.is_maximized().unwrap_or(false) {
                    let _ = w.unmaximize();
                } else {
                    let _ = w.maximize();
                }
            }
        }
    } else if action == "copy" || action == "paste" {
        let is_paste = action == "paste";
        let (selected, target, xdotool, own_pid, xdotool_children, prev_view) = {
            let st = state_arc.lock().unwrap();
            let sel_ids = ids(payload.get("ids")).map_err(|e| e.message)?;
            let mut selected = Vec::with_capacity(sel_ids.len());
            for id in &sel_ids {
                selected.push(st.store.item(id).map_err(|e| e.message)?);
            }
            if selected.is_empty() {
                return Err("Select an item first".to_string());
            }
            let target = st.paste_targets.resolve(st.previous_target.as_ref());
            let xdt = if st.session.backend == "x11" {
                st.tools.xdotool.clone()
            } else {
                None
            };
            (
                selected,
                target,
                xdt,
                st.paste_targets.own_pid,
                st.xdotool_children.clone(),
                st.view.clone(),
            )
        };
        write_selected_to_clipboard(&app, &selected).await?;
        let keep_open = payload
            .get("keepOpen")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_paste {
            hide_view(&app, false).await;
            let paste_res = tokio::task::spawn_blocking(move || {
                if target.is_some() {
                    xdotool_restore(
                        xdotool.as_deref(),
                        target.as_ref(),
                        own_pid,
                        Some(&xdotool_children),
                    );
                }
                xdotool_paste(
                    xdotool.as_deref(),
                    target.as_ref(),
                    own_pid,
                    Some(&xdotool_children),
                )
            })
            .await
            .map_err(|e| e.to_string())?;
            if let Err(e) = paste_res {
                show_view(&app, &prev_view, None).await;
                return Err(e);
            }
        } else if keep_open {
            notify_message(&app, "Copied");
        } else {
            hide_view(&app, true).await;
        }
    } else if action == "cancelTrim" {
        if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
            if let Some(flag) = state_arc.lock().unwrap().recording_trims.get(id) {
                flag.store(true, Ordering::SeqCst);
            }
        }
        cmd_result = Some(json!({ "canceled": true }));
    } else if action == "trimRecording" {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Select an available recording.".to_string())?
            .to_string();
        let start_ms = payload
            .get("startMs")
            .and_then(|v| v.as_f64())
            .unwrap_or(-1.0);
        let end_ms = payload.get("endMs").and_then(|v| v.as_f64()).unwrap_or(-1.0);
        let src_fp = payload
            .get("sourceFingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let (media_path, media_jobs, abort_flag) = {
            let mut st = state_arc.lock().unwrap();
            let item = st.store.item(&id).map_err(|e| e.message)?;
            let is_rec = item
                .get("recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let trashed = item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
            if !is_rec || trashed || st.recording_trims.contains_key(&id) {
                return Err("Select an available recording.".to_string());
            }
            let cur_vid = item
                .get("renderedVideoFile")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| item.get("videoFile").and_then(|v| v.as_str()))
                .unwrap_or("");
            if cur_vid.len() < 64 || &cur_vid[..64] != src_fp {
                return Err("The recording changed. Reopen its preview before trimming.".to_string());
            }
            let dur = item
                .get("durationMs")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if !start_ms.is_finite()
                || !end_ms.is_finite()
                || start_ms < 0.0
                || end_ms > dur
                || end_ms - start_ms < 50.0
            {
                return Err("Choose a nonempty interval within this recording.".to_string());
            }
            let mp = st.store.media_path(&id, false).map_err(|e| e.message)?;
            let flag = Arc::new(AtomicBool::new(false));
            st.recording_trims.insert(id.clone(), flag.clone());
            (mp, st.media_jobs.clone(), flag)
        };

        let mj_clone = media_jobs.clone();
        let flag_clone = abort_flag.clone();
        let trim_res = tokio::task::spawn_blocking(move || {
            mj_clone.execute(
                "Trim",
                &media_path,
                Some(start_ms),
                Some(end_ms),
                false,
                Some(&flag_clone),
            )
        })
        .await
        .map_err(|e| e.to_string())?;

        let commit_res = (|| -> Result<Value, String> {
            let mut trimmed = trim_res?;
            if abort_flag.load(Ordering::SeqCst) {
                media_jobs.release(&trimmed);
                return Err("Video operation canceled.".to_string());
            }
            {
                let obj = trimmed.as_object_mut().unwrap();
                obj.insert("id".to_string(), json!(id));
                obj.insert("sourceFingerprint".to_string(), json!(src_fp));
            }
            let commit = {
                let mut st = state_arc.lock().unwrap();
                st.store
                    .commit_recording_trim(
                        &trimmed,
                        Some(&|| abort_flag.load(Ordering::SeqCst)),
                    )
                    .map_err(|e| e.message)
            };
            media_jobs.release(&trimmed);
            commit?;
            trigger_image_search_refresh(&app);
            Ok(json!({ "id": id }))
        })();

        state_arc.lock().unwrap().recording_trims.remove(&id);
        cmd_result = Some(commit_res?);
    } else if action == "openLink" {
        let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let url = {
            let st = state_arc.lock().unwrap();
            let item = st.store.item(id).map_err(|e| e.message)?;
            if item.get("kind").and_then(|v| v.as_str()) != Some("link") {
                return Err("Link unavailable".to_string());
            }
            let raw_url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            plain_url(raw_url).ok_or_else(|| "Link unavailable".to_string())?
        };
        open_external_url_or_path(&url)?;
    } else if action == "openFile" || action == "revealFile" {
        let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let file = {
            let st = state_arc.lock().unwrap();
            let item = st.store.item(id).map_err(|e| e.message)?;
            if item.get("kind").and_then(|v| v.as_str()) != Some("image") {
                return Err("Select an image first.".to_string());
            }
            st.store.media_path(id, false).map_err(|e| e.message)?
        };
        if action == "revealFile" {
            reveal_file_in_folder(&file);
        } else {
            open_external_url_or_path(&file.to_string_lossy())?;
        }
    } else if action == "refreshLink" {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        {
            let st = state_arc.lock().unwrap();
            let item = st.store.item(&id).map_err(|e| e.message)?;
            if item.get("kind").and_then(|v| v.as_str()) != Some("link") {
                return Err("Select a link first.".to_string());
            }
            let enabled = st
                .store
                .data
                .get("settings")
                .and_then(|s| s.get("fetchLinkMetadata"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !enabled {
                return Err(
                    "Enable link metadata in Sources before refreshing link previews.".to_string(),
                );
            }
        }
        enrich_link(&app, &id, true).await?;
        cmd_result = Some(json!({ "id": id }));
    } else if action == "capture" {
        cmd_result = Some(capture_screenshot_action(&app).await?);
    } else if action == "newNote" {
        let text_arg = payload.get("text").and_then(|v| v.as_str());
        let title_arg = payload.get("title").and_then(|v| v.as_str());
        let note = state_arc
            .lock()
            .unwrap()
            .store
            .create_note(text_arg, title_arg)
            .map_err(|e| e.message)?;
        let id = note.get("id").and_then(|v| v.as_str()).unwrap_or("");
        cmd_result = Some(json!({ "id": id }));
    } else if action == "import" {
        let files =
            tokio::task::spawn_blocking(|| pick_files_native(false, true))
                .await
                .unwrap_or_default();
        let mut imported = Vec::new();
        let mut failures = Vec::new();
        for file in files {
            match import_single_file(&app, &file) {
                Ok(item) => {
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let kind = item
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    imported.push(id.clone());
                    if kind == "link" {
                        let app_c = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = enrich_link(&app_c, &id, false).await;
                        });
                    }
                }
                Err(e) => {
                    let base = file
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file");
                    failures.push(format!("{}: {}", base, e));
                }
            }
        }
        if !failures.is_empty() {
            let prefix = if !imported.is_empty() {
                format!("Imported {} · failed {}. ", imported.len(), failures.len())
            } else {
                String::new()
            };
            state_arc.lock().unwrap().message = format!("{}{}", prefix, failures[0]);
        }
        cmd_result = Some(json!({ "ids": imported, "failed": failures.len() }));
    } else if action == "searchImages" {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Search images with up to 2,048 characters.".to_string())?
            .to_string();
        let eligible_val = payload.get("eligibleIds").cloned();
        let (worker, items) = {
            let st = state_arc.lock().unwrap();
            let items = st
                .store
                .data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            (st.image_search_worker.clone(), items)
        };
        let search_res = tokio::task::spawn_blocking(move || {
            search_images_with_worker(&worker, &items, &query, eligible_val.as_ref())
        })
        .await
        .map_err(|e| e.to_string())??;
        cmd_result = Some(search_res);
    } else if action == "ocr" {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        cmd_result = Some(run_ocr_for_item(&app, &id, false).await?);
    } else if action == "copyOCR" {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let recognized = run_ocr_for_item(&app, &id, false).await?;
        let text = recognized
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if text.trim().is_empty() {
            return Err("No text found in this image.".to_string());
        }
        write_selected_to_clipboard(&app, &[json!({ "kind": "text", "text": text })]).await?;
        notify_message(&app, "Recognized text copied");
        cmd_result = Some(recognized);
    } else if action == "export" {
        let selected = {
            let st = state_arc.lock().unwrap();
            let sel_ids = ids(payload.get("ids")).map_err(|e| e.message)?;
            let mut items = Vec::with_capacity(sel_ids.len());
            for id in &sel_ids {
                items.push(st.store.item(id).map_err(|e| e.message)?);
            }
            if items.is_empty() {
                return Err("Select an item first".to_string());
            }
            items
        };
        let dirs =
            tokio::task::spawn_blocking(|| pick_files_native(true, false))
                .await
                .unwrap_or_default();
        if let Some(dir) = dirs.first() {
            let mut st = state_arc.lock().unwrap();
            let exported = export_copies(&selected, dir, "linux", |id| {
                st.store.media_path(id, false).map_err(|e| e.message)
            })?;
            st.message = exported.message.clone();
            cmd_result = Some(json!({ "count": exported.count, "failed": exported.failed }));
        }
    } else if action == "startDrag" {
        let mut st = state_arc.lock().unwrap();
        let sel_ids = ids(payload.get("ids")).map_err(|e| e.message)?;
        if sel_ids.is_empty() || sel_ids.len() > 1000 {
            return Err("Select up to 1000 items to drag.".to_string());
        }
        let mut selected = Vec::with_capacity(sel_ids.len());
        for id in &sel_ids {
            selected.push(st.store.item(id).map_err(|e| e.message)?);
        }
        if st.drag_files.is_none() {
            st.drag_files = Some(DragFiles::new(&st.store.directory)?);
        }
        let store_dir = st.store.directory.clone();
        let mut df = st.drag_files.take().unwrap_or_else(|| DragFiles::new(&store_dir).unwrap());
        let prep_res = df.prepare(&selected, "linux", |id| {
            st.store.media_path(id, false).map_err(|e| e.message)
        });
        st.drag_files = Some(df);
        let _ = prep_res?;
    } else if action == "setSettings" {
        let mut st = state_arc.lock().unwrap();
        if let Some(rec_tok) = payload.get("recordingToken") {
            let Some(tok_str) = rec_tok.as_str() else {
                return Err("Invalid shortcut recorder.".to_string());
            };
            if st.shortcut_recording.as_ref().map(|(t, _)| t.as_str()) == Some(tok_str) {
                if let Some((_, lease)) = st.shortcut_recording.take() {
                    if let Some(ref mut sc) = st.shortcuts {
                        sc.resume(lease);
                    }
                }
            }
        }
        let Some(patch_obj) = payload.get("patch").and_then(|v| v.as_object()) else {
            return Err("Invalid settings".to_string());
        };
        if patch_obj.contains_key("watchFolders") {
            return Err("Choose watched folders with the folder picker.".to_string());
        }
        let was_indexing = st
            .store
            .data
            .get("settings")
            .and_then(|s| s.get("indexImages"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let prev_shortcuts: HashMap<String, String> = st
            .store
            .data
            .get("settings")
            .and_then(|s| s.get("shortcuts"))
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(launch_val) = patch_obj.get("launchAtLogin").and_then(|v| v.as_bool()) {
            if !st.test_mode {
                let exe = std::env::current_exe()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "clipboardline".to_string());
                let cmd = launch_command(&st.env_map, &exe, true, None);
                set_autostart(launch_val, &cmd, &st.env_map, None)?;
            }
        }

        st.store
            .modify("setSettings", &json!({ "patch": patch_obj }))
            .map_err(|e| e.message)?;

        let now_indexing = st
            .store
            .data
            .get("settings")
            .and_then(|s| s.get("indexImages"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if was_indexing && !now_indexing {
            st.ocr_queue.discard_pending();
        }
        if patch_obj.contains_key("shortcuts") {
            let new_shortcuts: HashMap<String, String> = st
                .store
                .data
                .get("settings")
                .and_then(|s| s.get("shortcuts"))
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let rejected = if let Some(ref mut sc) = st.shortcuts {
                sc.update(&new_shortcuts)
            } else {
                Vec::new()
            };
            if !rejected.is_empty() {
                let mut revert_patch = serde_json::Map::new();
                for r in &rejected {
                    let prev_val = prev_shortcuts.get(&r.action).cloned().unwrap_or_default();
                    revert_patch.insert(r.action.clone(), json!(prev_val));
                }
                let _ = st.store.modify(
                    "setSettings",
                    &json!({ "patch": { "shortcuts": revert_patch } }),
                );
                let sc_names: Vec<String> = rejected.iter().map(|r| r.shortcut.clone()).collect();
                let unrestored: Vec<String> = rejected
                    .iter()
                    .filter(|r| r.restoration_failed == Some(true))
                    .map(|r| r.action.clone())
                    .collect();
                let suffix = if !unrestored.is_empty() {
                    format!("Could not restore: {}.", unrestored.join(", "))
                } else if rejected.iter().any(|r| !r.previous.is_empty()) {
                    "Working previous shortcuts are kept.".to_string()
                } else {
                    "Choose another combination in Settings.".to_string()
                };
                st.message = format!("Shortcut {} is unavailable. {}", sc_names.join(", "), suffix);
                cmd_result = Some(json!({ "error": st.message }));
            }
        }
        if ["captureText", "captureImages", "captureLinks"]
            .iter()
            .any(|k| patch_obj.contains_key(*k))
        {
            st.last_sequence = None;
            st.last_fingerprint.clear();
            let settings = st.store.data.get("settings").cloned().unwrap_or_default();
            let want_text = settings
                .get("captureText")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || settings
                    .get("captureLinks")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
            let want_img = settings
                .get("captureImages")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(ref watchers) = st.wayland_watchers {
                watchers.update(want_text, want_img);
            }
        }
        if patch_obj.contains_key("lineFullScreen") && st.view == "shelves" {
            let line_full_screen = st
                .store
                .data
                .get("settings")
                .and_then(|s| s.get("lineFullScreen"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_fullscreen(line_full_screen);
            }
        }
        if patch_obj.contains_key("retentionDays") {
            drop(st);
            trigger_image_search_refresh(&app);
        }
    } else if action == "watchFolder" || action == "chooseWatchFolder" {
        let dirs =
            tokio::task::spawn_blocking(|| pick_files_native(true, false))
                .await
                .unwrap_or_default();
        if let Some(dir) = dirs.first() {
            let mut st = state_arc.lock().unwrap();
            let mut folders: Vec<Value> = st
                .store
                .data
                .get("settings")
                .and_then(|s| s.get("watchFolders"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            folders.push(json!(dir.to_string_lossy().to_string()));
            st.store
                .modify("setSettings", &json!({ "patch": { "watchFolders": folders } }))
                .map_err(|e| e.message)?;
        }
    } else if action == "unwatchFolder" {
        let Some(index) = payload.get("index").and_then(|v| v.as_i64()) else {
            return Err("Invalid watched folder".to_string());
        };
        let mut st = state_arc.lock().unwrap();
        let folders: Vec<Value> = st
            .store
            .data
            .get("settings")
            .and_then(|s| s.get("watchFolders"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if index < 0 || (index as usize) >= folders.len() {
            return Err("Invalid watched folder".to_string());
        }
        let filtered: Vec<Value> = folders
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i != (index as usize))
            .map(|(_, v)| v)
            .collect();
        st.store
            .modify(
                "setSettings",
                &json!({ "patch": { "watchFolders": filtered } }),
            )
            .map_err(|e| e.message)?;
    } else if action == "openReleases" {
        open_external_url_or_path(RELEASES_URL)?;
    } else if action == "checkUpdates" || action == "refreshUpdates" {
        // Manual updates on Linux
    } else if action == "installUpdate" {
        return Err("Linux packages are updated by installing a newer release.".to_string());
    } else if action == "quit" {
        app.exit(0);
    } else {
        return Err("Unknown command".to_string());
    }

    let surface_only = [
        "show",
        "search",
        "hide",
        "library",
        "settings",
        "windowClose",
        "windowMinimize",
        "windowMaximize",
    ]
    .contains(&action.as_str());

    Ok(broadcast_state(&app, surface_only, cmd_result))
}

fn handle_appmedia_request(
    app: &AppHandle,
    request: &tauri::http::Request<Vec<u8>>,
) -> HttpResponse<Vec<u8>> {
    let method = request.method().as_str();
    let origin_hdr = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok());
    let Some(origin) = media_origin(method, origin_hdr, None) else {
        return HttpResponse::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Vec::new())
            .unwrap();
    };

    let uri = request.uri();
    let host = uri.host().unwrap_or("");
    let path = uri.path();
    let query = uri.query();

    if host != "media" {
        return HttpResponse::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new())
            .unwrap();
    }

    let Some(caps) = media_route_re().captures(path) else {
        return HttpResponse::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new())
            .unwrap();
    };
    let id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let route = caps.get(2).map(|m| m.as_str());

    let state_arc = app.state::<SharedState>();
    let file_res = {
        let st = state_arc.lock().unwrap();
        match route {
            Some("/thumbnail") => st.store.thumbnail_path(id).map_err(|e| e.message),
            Some("/video") | Some("/video/original") => {
                let item = st.store.item(id);
                if item
                    .as_ref()
                    .map(|i| {
                        i.get("recording")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
                {
                    st.store
                        .media_path(id, route == Some("/video/original"))
                        .map_err(|e| e.message)
                } else {
                    Err("Media unavailable".to_string())
                }
            }
            _ => st
                .store
                .poster_path(id, route == Some("/original"))
                .map_err(|e| e.message),
        }
    };

    let Ok(file) = file_res else {
        return HttpResponse::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new())
            .unwrap();
    };

    if !media_generation_matches(query, route, &file) {
        return HttpResponse::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new())
            .unwrap();
    }

    let is_video = route.map(|r| r.starts_with("/video")).unwrap_or(false);
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let mime = if is_video {
        if ext == "mov" {
            "video/quicktime"
        } else {
            "video/mp4"
        }
    } else {
        "image/png"
    };

    let range_hdr = if is_video {
        request
            .headers()
            .get("range")
            .and_then(|v| v.to_str().ok())
    } else {
        None
    };

    match build_media_response(&file, mime, &origin, range_hdr) {
        Ok(built) => {
            let mut builder = HttpResponse::builder().status(built.status);
            for (k, v) in built.headers {
                builder = builder.header(k, v);
            }
            builder.body(built.body).unwrap()
        }
        Err(_) => HttpResponse::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new())
            .unwrap(),
    }
}

fn parse_cli_linux_action(argv: &[String]) -> Option<&'static str> {
    for arg in argv {
        match arg.as_str() {
            "--toggle" => return Some("toggle"),
            "--search" => return Some("search"),
            "--library" => return Some("library"),
            "--copy-latest" => return Some("copyLatest"),
            "--screenshot" => return Some("screenshot"),
            _ => {}
        }
    }
    None
}

pub fn run() {
    let argv: Vec<String> = std::env::args().collect();
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let session = linux_session(&env_map, &argv);

    if std::env::var_os("GDK_BACKEND").is_none() {
        std::env::set_var("GDK_BACKEND", &session.backend);
    }

    let tools = linux_tools(&env_map);
    let reveal_delay = map_animation_delay(&session, &env_map).max(80);
    let test_mode = argv.iter().any(|a| a == "--test-mode");
    let test_folder = argv
        .iter()
        .find_map(|a| a.strip_prefix("--test-data="))
        .map(PathBuf::from);
    let development = cfg!(debug_assertions) && !test_mode;

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let app_clone = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(act) = parse_cli_linux_action(&args) {
                    handle_global_action(&app_clone, act).await;
                } else {
                    show_view(&app_clone, "shelves", None).await;
                }
            });
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol("appmedia", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            tauri::async_runtime::spawn(async move {
                let response = handle_appmedia_request(&app, &request);
                responder.respond(response);
            });
        })
        .invoke_handler(tauri::generate_handler![
            clipboardline_state,
            clipboardline_command,
            clipboardline_shelf_motion_finished
        ])
        .setup(move |app| {
            let data_dir = if let Some(ref tf) = test_folder {
                tf.clone()
            } else if development {
                app.path()
                    .app_config_dir()
                    .unwrap_or_else(|_| PathBuf::from("/tmp/clipboardline-dev"))
                    .parent()
                    .unwrap_or_else(|| Path::new("/tmp"))
                    .join("ClipBoardLine Dev")
            } else {
                app.path()
                    .app_data_dir()
                    .unwrap_or_else(|_| PathBuf::from("/tmp/clipboardline"))
            };

            let store = Store::new(&data_dir).expect("Failed to initialize ClipBoardLine store");
            let media_jobs = Arc::new(
                MediaJobs::new(
                    data_dir.join("video-jobs"),
                    tools.ffmpeg.clone(),
                    tools.ffprobe.clone(),
                    MAXIMUM_JOB_MS,
                )
                .expect("Failed to initialize MediaJobs"),
            );

            let resource_dir = app
                .path()
                .resource_dir()
                .unwrap_or_else(|_| PathBuf::from("."));
            let model_dir = if resource_dir.join("CLIP").exists() {
                resource_dir.join("CLIP")
            } else {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("assets")
                    .join("CLIP")
            };
            let cache_dir = data_dir.join("image-search");
            let worker = ImageSearchWorker::new(model_dir, cache_dir);
            let worker_avail = worker.available;
            let image_search_state = json!({
                "available": worker_avail,
                "indexing": false,
                "pending": 0,
                "revision": 0,
                "error": if worker_avail {
                    Value::Null
                } else {
                    json!("Offline image search is unavailable. Reinstall the official application to restore its bundled models.")
                }
            });

            let copy_marker: [u8; 16] = rand::random();
            let wayland = session.backend == "wayland";
            let (initial_mode, initial_problem) = if wayland {
                if tools.wl_paste.is_some() {
                    ("wl-clipboard".to_string(), String::new())
                } else {
                    (
                        "focused-polling".to_string(),
                        "Install wl-clipboard for background clipboard capture on Wayland."
                            .to_string(),
                    )
                }
            } else {
                ("x11-polling".to_string(), String::new())
            };

            let runtime_state = AppRuntimeState {
                store,
                session: session.clone(),
                tools: tools.clone(),
                env_map: env_map.clone(),
                reveal_delay,
                view: "shelves".to_string(),
                view_event: None,
                message: String::new(),
                desired_visible: false,
                visibility_generation: 0,
                paste_generation: 0,
                last_broadcast_revision: None,
                clipboard_monitor_mode: initial_mode,
                clipboard_problem: initial_problem,
                last_sequence: None,
                current_sequence: 1,
                last_fingerprint: String::new(),
                last_ids: Vec::new(),
                own_fingerprint: String::new(),
                kept_clipboard: None,
                own_writes: 0,
                capturing: false,
                pending_clipboard: false,
                paste_targets: LinuxPasteTargets::new(std::process::id()),
                previous_target: None,
                xdotool_children: Arc::new(Mutex::new(Vec::new())),
                copy_marker,
                ocr_queue: OcrQueue::new(),
                active_ocr: HashSet::new(),
                enrich_generations: HashMap::new(),
                next_enrich_gen: 0,
                recording_trims: HashMap::new(),
                shortcut_recording: None,
                drag_files: None,
                media_jobs,
                image_search_worker: Arc::new(Mutex::new(worker)),
                image_search_state,
                motion_bridge: Arc::new(Mutex::new(MotionBridge::new())),
                shortcuts: None,
                wayland_watchers: None,
                last_tray_key: String::new(),
                last_tray_icon_key: String::new(),
                last_tray_menu_key: String::new(),
                test_mode,
                development,
            };

            let shared_state: SharedState = Arc::new(Mutex::new(runtime_state));
            app.manage(shared_state.clone());

            let win = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("ClipBoardLine")
            .inner_size(1180.0, 340.0)
            .visible(false)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .initialization_script(BRIDGE_INIT_SCRIPT)
            .build()?;

            let app_for_win = app.handle().clone();
            win.on_window_event(move |event| match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let app_c = app_for_win.clone();
                    tauri::async_runtime::spawn(async move {
                        hide_view(&app_c, true).await;
                    });
                }
                WindowEvent::Focused(false) => {
                    let app_c = app_for_win.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let should_dismiss = {
                            let st_arc = app_c.state::<SharedState>();
                            let st = st_arc.lock().unwrap();
                            st.session.backend == "wayland"
                                && st.desired_visible
                                && st.view == "shelves"
                        };
                        if should_dismiss {
                            if let Some(w) = app_c.get_webview_window("main") {
                                if !w.is_focused().unwrap_or(false) {
                                    hide_view(&app_c, false).await;
                                }
                            }
                        }
                    });
                }
                _ => {}
            });

            // Initialize Tray
            if !test_mode {
                let menu = build_tray_menu(app.handle(), "en")?;
                let default_icon = TauriImage::from_bytes(BASE_TRAY_ICON_PNG)?;
                let app_for_tray = app.handle().clone();
                let app_for_menu = app.handle().clone();
                let _ = TrayIconBuilder::with_id("main-tray")
                    .icon(default_icon)
                    .menu(&menu)
                    .on_menu_event(move |_app, event| {
                        let id = event.id().as_ref().to_string();
                        let app_c = app_for_menu.clone();
                        tauri::async_runtime::spawn(async move {
                            match id.as_str() {
                                "tray-show" => show_view(&app_c, "shelves", None).await,
                                "tray-library" => show_view(&app_c, "library", None).await,
                                "tray-capture" => {
                                    let _ = capture_screenshot_action(&app_c).await;
                                }
                                "tray-settings" | "tray-check" => {
                                    show_view(
                                        &app_c,
                                        "settings",
                                        Some(json!({ "settingsTab": "updates" })),
                                    )
                                    .await;
                                }
                                "tray-quit" => app_c.exit(0),
                                _ => {}
                            }
                        });
                    })
                    .on_tray_icon_event(move |_tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app_c = app_for_tray.clone();
                            tauri::async_runtime::spawn(async move {
                                handle_global_action(&app_c, "toggle").await;
                            });
                        }
                    })
                    .build(app)?;
                refresh_tray(app.handle());
            }

            // Initialize GlobalShortcuts
            if !test_mode {
                let app_handle = app.handle().clone();
                let app_err = app.handle().clone();
                let mut sc_mgr = GlobalShortcuts::new(
                    TauriShortcutAdapter { app: app_handle },
                    Box::new(move |msg| notify_message(&app_err, msg)),
                );
                let configured_map: HashMap<String, String> = {
                    let st = shared_state.lock().unwrap();
                    st.store
                        .data
                        .get("settings")
                        .and_then(|s| s.get("shortcuts"))
                        .and_then(|v| v.as_object())
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                sc_mgr.update(&configured_map);
                shared_state.lock().unwrap().shortcuts = Some(sc_mgr);
            }

            // Initialize Clipboard Monitor / Poller
            let app_clip = app.handle().clone();
            if wayland && tools.wl_paste.is_some() {
                let wl_paste_bin = tools.wl_paste.clone().unwrap();
                let app_chg = app_clip.clone();
                let app_fail = app_clip.clone();
                let watchers = Arc::new(WaylandWatchers::new(
                    wl_paste_bin,
                    Arc::new(move |_kind| {
                        {
                            let st_arc = app_chg.state::<SharedState>();
                            let mut st = st_arc.lock().unwrap();
                            st.current_sequence = if st.current_sequence >= 0xffff_ffff {
                                1
                            } else {
                                st.current_sequence + 1
                            };
                        }
                        let app_c = app_chg.clone();
                        tauri::async_runtime::spawn(async move {
                            poll_and_capture_clipboard(&app_c).await;
                        });
                    }),
                    Arc::new(move |msg, _unsupported| {
                        {
                            let st_arc = app_fail.state::<SharedState>();
                            let mut st = st_arc.lock().unwrap();
                            st.clipboard_monitor_mode = "focused-polling".to_string();
                            st.clipboard_problem = msg.clone();
                        }
                        notify_message(
                            &app_fail,
                            format!(
                                "{} Clipboard capture now works only while ClipBoardLine is focused.",
                                msg
                            ),
                        );
                    }),
                ));
                let (want_text, want_img) = {
                    let st = shared_state.lock().unwrap();
                    let s = st.store.data.get("settings").cloned().unwrap_or_default();
                    let t = s
                        .get("captureText")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || s.get("captureLinks")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                    let im = s
                        .get("captureImages")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    (t, im)
                };
                watchers.update(want_text, want_img);
                shared_state.lock().unwrap().wayland_watchers = Some(watchers);
            } else {
                let app_poll = app_clip.clone();
                tauri::async_runtime::spawn(async move {
                    let mut last_sig: Option<String> = None;
                    loop {
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        let (sess, tls, want_any, want_img) = {
                            let st_arc = app_poll.state::<SharedState>();
                            let st = st_arc.lock().unwrap();
                            let s = st.store.data.get("settings").cloned().unwrap_or_default();
                            let t = s
                                .get("captureText")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                                || s.get("captureLinks")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(true);
                            let im = s
                                .get("captureImages")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            (st.session.clone(), st.tools.clone(), t || im, im)
                        };
                        if !want_any {
                            continue;
                        }
                        let sig_res = tokio::task::spawn_blocking(move || {
                            if sess.backend == "wayland" {
                                let Some(ref wp) = tls.wl_paste else {
                                    return Err("missing".to_string());
                                };
                                clipboard_signature(&WaylandClipboard::new(wp), want_img)
                            } else {
                                let Some(ref xc) = tls.xclip else {
                                    return Err("missing".to_string());
                                };
                                clipboard_signature(&X11Clipboard::new(xc), want_img)
                            }
                        })
                        .await
                        .ok()
                        .and_then(|r| r.ok());

                        if let Some(sig) = sig_res {
                            if last_sig.as_ref() != Some(&sig) {
                                last_sig = Some(sig);
                                {
                                    let st_arc = app_poll.state::<SharedState>();
                                    let mut st = st_arc.lock().unwrap();
                                    st.current_sequence = if st.current_sequence >= 0xffff_ffff {
                                        1
                                    } else {
                                        st.current_sequence + 1
                                    };
                                }
                                poll_and_capture_clipboard(&app_poll).await;
                            }
                        }
                    }
                });
            }

            // Hourly retention expiration timer
            let app_exp = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    let expired = {
                        let st_arc = app_exp.state::<SharedState>();
                        let mut st = st_arc.lock().unwrap();
                        st.store.expire(true).unwrap_or(false)
                    };
                    if expired {
                        broadcast_state(&app_exp, false, None);
                    }
                }
            });

            trigger_image_search_refresh(app.handle());

            if let Some(initial_act) = parse_cli_linux_action(&argv) {
                let app_act = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    handle_global_action(&app_act, initial_act).await;
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ClipBoardLine Tauri application");
}
