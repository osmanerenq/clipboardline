use crate::clipboard_policy::{
    private_clipboard, read_clipboard_content, repeated_generation, valid_sequence,
    ClipboardEntry, ClipboardPayload, ClipboardReadResult, ClipboardReader,
};
use crate::export_drag::{
    decode_text_file, export_copies, link_file, sanitize_export_label, DragFiles,
};
use crate::image_search::{
    image_regions, image_tensors, normalized, ranked_matches, score, ScoredItem, DIMENSIONS,
};
use crate::linux_clipboard::{
    decodable_png, file_clipboard, kept_copy, parse_type_lines, pixel_fingerprint, KeptClipboard,
};
use crate::linux_media::{
    choose_encoders, describe, encoder_attempts, trim_arguments, INPUT_ARGS,
};
use crate::linux_session::{
    autostart_enabled, autostart_entry, autostart_file, exec_argument, linux_session,
    parse_portal_response, screenshot_path, set_autostart, PortalResponse,
};
use crate::media_protocol::{
    build_media_response, byte_range, media_generation_matches, media_origin,
};
use crate::model::{
    default_settings, folded_name, plain_url, sized_text, Store,
};
use crate::network::{
    collapse, decode_entities, decode_html, literal_destination, metadata_guard,
    parse_html_metadata, public_address, public_url,
};
use crate::ocr::OcrQueue;
use crate::shelf_motion::{MotionBridge, RectBounds, WindowBoundsTracker};
use crate::shortcuts::{GlobalShortcuts, ShortcutRegistry};
use crate::status_tray::{badge_bitmap, language_code, status_summary, translated};
use image::{ImageBuffer, Rgba};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::net::IpAddr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;
use uuid::Uuid;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("clipboardline-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> TempDir {
    TempDir::new()
}

fn make_test_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_fn(width, height, |_, _| Rgba(rgba));
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

#[test]
fn store_preserves_line_full_screen_and_rejects_symlinks() {
    let dir = tempdir();
    let data_dir = dir.path().join("data");
    let mut store = Store::new(&data_dir).unwrap();
    assert_eq!(
        store.data["settings"]["lineFullScreen"].as_bool(),
        Some(false),
        "lineFullScreen defaults to false"
    );

    store
        .modify("setSettings", &json!({ "patch": { "lineFullScreen": true, "sounds": true } }))
        .unwrap();
    assert_eq!(store.data["settings"]["lineFullScreen"].as_bool(), Some(true));

    let reloaded = Store::new(&data_dir).unwrap();
    assert_eq!(
        reloaded.data["settings"]["lineFullScreen"].as_bool(),
        Some(true),
        "lineFullScreen persists across reloads"
    );
    assert_eq!(reloaded.data["settings"]["sounds"].as_bool(), Some(true));

    let symlink_dir = dir.path().join("symlink-data");
    symlink(&data_dir, &symlink_dir).unwrap();
    let err = Store::new(&symlink_dir).err().expect("symlinked storage directory must be rejected");
    assert!(err.message.contains("symlink"));
}

#[test]
fn model_validates_urls_utf8_limits_tags_and_notes() {
    assert_eq!(
        plain_url("https://example.com/path?x=1"),
        Some("https://example.com/path?x=1".to_string())
    );
    assert_eq!(plain_url("javascript:alert(1)"), None);
    assert_eq!(plain_url(&format!("{}//u:p{}example.com/", "https:", "@")), None);
    assert_eq!(plain_url("https://example.com:99999/"), None);
    assert_eq!(plain_url("http:///path"), None);

    assert!(sized_text(Some(&json!("hello")), false).is_ok());
    let over_1mib = "a".repeat(1024 * 1024 + 1);
    assert!(sized_text(Some(&json!(over_1mib)), false).is_err());

    assert_eq!(folded_name("Work Notes"), "work notes");

    let dir = tempdir();
    let mut store = Store::new(dir.path().join("store")).unwrap();
    let note = store
        .create_note(Some("Milk\nEggs"), Some("Shopping"))
        .unwrap();
    assert_eq!(note["createdAsNote"].as_bool(), Some(true));
    assert_eq!(note["name"].as_str(), Some("Shopping"));
    let note_id = note["id"].as_str().unwrap().to_string();

    // Tag folding and 64-grapheme tag rejection
    store
        .modify("tag", &json!({ "id": note_id, "tags": ["Urgent", "urgent"] }))
        .unwrap();
    let tags = store.item(&note_id).unwrap()["tags"].as_array().unwrap().clone();
    assert_eq!(tags, vec![json!("Urgent")]);

    let long_tag = "x".repeat(65);
    assert!(store
        .modify("tag", &json!({ "id": note_id, "tags": [long_tag] }))
        .is_err());

    // Undoable trash and restore
    let trashed = store
        .modify("trashUndoably", &json!({ "ids": [note_id] }))
        .unwrap();
    assert_eq!(trashed["ids"], json!([note_id]));
    assert!(store.item(&note_id).unwrap()["trashedAt"].is_number());

    store
        .modify("undoTrash", &json!({}))
        .unwrap();
    assert!(store.item(&note_id).unwrap()["trashedAt"].is_null());

    // Image recopy deduplication
    let png = make_test_png(4, 4, [20, 40, 80, 255]);
    let img1 = store
        .insert_image(&png, Some("Image"))
        .unwrap();
    let img2 = store
        .insert_image(&png, Some("Image"))
        .unwrap();
    assert_eq!(img1["id"], img2["id"], "recopied identical image reuses item");
}

struct FakeClipEntry {
    mime_types: Vec<String>,
    data: HashMap<String, Vec<u8>>,
}

impl ClipboardEntry for FakeClipEntry {
    fn types(&self) -> Vec<String> {
        self.mime_types.clone()
    }
    fn get_type(&self, mime: &str) -> Result<Vec<u8>, String> {
        self.data
            .get(mime)
            .cloned()
            .ok_or_else(|| "missing".to_string())
    }
}

struct FakeClipReader {
    entries: Vec<FakeClipEntry>,
}

impl ClipboardReader for FakeClipReader {
    type Entry = FakeClipEntry;
    fn read(&self) -> Result<Vec<Self::Entry>, String> {
        Ok(self
            .entries
            .iter()
            .map(|e| FakeClipEntry {
                mime_types: e.mime_types.clone(),
                data: e.data.clone(),
            })
            .collect())
    }
}

#[test]
fn clipboard_policy_blocks_concealed_self_marker_and_raced_reads() {
    assert!(private_clipboard(&["text/plain", "org.nspasteboard.ConcealedType"]));
    assert!(private_clipboard(&["x-kde-passwordManagerHint"]));
    assert!(!private_clipboard(&["text/plain", "UTF8_STRING"]));
    assert!(!valid_sequence(0));
    assert!(valid_sequence(42));

    let marker = b"marker-secret-bytes";
    let mut data = HashMap::new();
    data.insert("text/plain".to_string(), b"https://example.com/hello".to_vec());
    let reader = FakeClipReader {
        entries: vec![FakeClipEntry {
            mime_types: vec!["text/plain".to_string()],
            data,
        }],
    };
    let res = read_clipboard_content(&reader, || Ok(7), marker, true);
    assert_eq!(
        res,
        ClipboardReadResult::Content(crate::clipboard_policy::ClipboardContent {
            kind: "link".to_string(),
            payload: ClipboardPayload::Link("https://example.com/hello".to_string()),
            sequence: 7,
            owned: false,
            fingerprint: match &res {
                ClipboardReadResult::Content(c) => c.fingerprint.clone(),
                _ => String::new(),
            },
        })
    );

    // Self-owned marker is ignored
    let mut owned_data = HashMap::new();
    owned_data.insert("text/plain".to_string(), b"copied from shelf".to_vec());
    owned_data.insert("web application/x.clipboardline".to_string(), marker.to_vec());
    let owned_reader = FakeClipReader {
        entries: vec![FakeClipEntry {
            mime_types: vec![
                "text/plain".to_string(),
                "web application/x.clipboardline".to_string(),
            ],
            data: owned_data,
        }],
    };
    assert_eq!(
        read_clipboard_content(&owned_reader, || Ok(8), marker, true),
        ClipboardReadResult::Empty
    );

    // Sequence change mid-read returns Raced
    let seq = AtomicU64::new(10);
    let raced_res = read_clipboard_content(
        &reader,
        || Ok(seq.fetch_add(1, Ordering::SeqCst)),
        marker,
        true,
    );
    assert_eq!(raced_res, ClipboardReadResult::Raced);

    let mut last = None;
    assert!(!repeated_generation(&mut last, 5));
    assert!(repeated_generation(&mut last, 5));
    assert!(!repeated_generation(&mut last, 6));
}

#[test]
fn linux_clipboard_validates_png_bombs_and_kept_image_copies() {
    let valid_png = make_test_png(2, 2, [255, 0, 128, 255]);
    assert!(decodable_png(&valid_png));
    let fp = pixel_fingerprint(&valid_png).expect("valid PNG produces pixel fingerprint");
    assert!(fp.starts_with("image:pixels:2x2:"));

    // PNG IHDR claiming 10000x10000 = 100 MP (> 80 MP limit) must be rejected before decode
    let mut bomb = valid_png.clone();
    bomb[16..20].copy_from_slice(&10_000u32.to_be_bytes());
    bomb[20..24].copy_from_slice(&10_000u32.to_be_bytes());
    assert!(!decodable_png(&bomb));
    assert_eq!(pixel_fingerprint(&bomb), None);

    let kept_img = KeptClipboard {
        id: "img-1".to_string(),
        kind: "image".to_string(),
        value: valid_png,
        fingerprint: fp.clone(),
    };
    let shelf_items = vec![json!({
        "id": "img-1",
        "kind": "image",
        "trashedAt": Value::Null,
        "hiddenFromShelf": false
    })];
    assert!(kept_copy(Some(&kept_img), &fp, &shelf_items).is_some());

    let trashed_items = vec![json!({
        "id": "img-1",
        "kind": "image",
        "trashedAt": "2026-11-10T00:00:00Z",
        "hiddenFromShelf": false
    })];
    assert!(kept_copy(Some(&kept_img), &fp, &trashed_items).is_none());

    let kept_text = KeptClipboard {
        id: "txt-1".to_string(),
        kind: "text".to_string(),
        value: b"secret".to_vec(),
        fingerprint: "text:abc".to_string(),
    };
    assert!(
        kept_copy(Some(&kept_text), "text:abc", &shelf_items).is_none(),
        "only images are kept in memory for Wayland/X11 owner re-serving"
    );

    let fc = file_clipboard(&[PathBuf::from("/tmp/hello world.png")]);
    assert_eq!(fc.uri_list, "file:///tmp/hello%20world.png\r\n");
    assert_eq!(fc.gnome, "copy\nfile:///tmp/hello%20world.png");
    assert_eq!(
        parse_type_lines(b"text/plain\nimage/png\ntext/plain\n", 10),
        vec!["text/plain".to_string(), "image/png".to_string()]
    );
}

#[test]
fn linux_session_autostart_and_portal_hardening() {
    let mut env = HashMap::new();
    env.insert("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string());
    env.insert("DISPLAY".to_string(), ":0".to_string());
    env.insert("XDG_CURRENT_DESKTOP".to_string(), "ubuntu:GNOME".to_string());
    let sess = linux_session(&env, &[]);
    assert_eq!(sess.session, "wayland");
    assert_eq!(sess.backend, "x11");
    assert_eq!(sess.reason, "mutter-selection-bridge");
    assert!(sess.mutter);

    assert_eq!(
        exec_argument("/home/u/Apps/Clip Board 100%.AppImage").unwrap(),
        "\"/home/u/Apps/Clip Board 100%%.AppImage\""
    );
    for unsafe_arg in ["/a\"b", "/a$b", "/a`b", "/a\\b", "/a\nb", "/a\x7fb", ""] {
        assert!(exec_argument(unsafe_arg).is_err());
    }

    let entry = autostart_entry(
        &["/opt/ClipBoardLine/clipboardline".to_string()],
        Some("ClipBoardLine\0\x1fApp"),
    )
    .unwrap();
    assert!(entry.contains("\nName=ClipBoardLine  App\n"));

    let dir = tempdir();
    let cfg_dir = dir.path().join("config");
    let mut auto_env = HashMap::new();
    auto_env.insert(
        "XDG_CONFIG_HOME".to_string(),
        cfg_dir.to_string_lossy().to_string(),
    );
    let file = autostart_file(&auto_env);
    assert!(set_autostart(
        true,
        &["/opt/ClipBoardLine/clipboardline".to_string()],
        &auto_env,
        None
    )
    .unwrap());
    assert!(autostart_enabled(&auto_env));
    assert!(!set_autostart(false, &[], &auto_env, None).unwrap());
    assert!(!file.exists());

    // Foreign autostart entry must not be overwritten or deleted
    fs::write(&file, "[Desktop Entry]\nExec=other\n").unwrap();
    assert!(set_autostart(true, &["/x".to_string()], &auto_env, None).is_err());
    assert!(set_autostart(false, &[], &auto_env, None).is_err());
    fs::remove_file(&file).unwrap();

    // Symlinked autostart directory must be rejected
    let sym_cfg = dir.path().join("sym-config");
    fs::create_dir_all(&sym_cfg).unwrap();
    let target_dir = dir.path().join("target-autostart");
    fs::create_dir_all(&target_dir).unwrap();
    symlink(&target_dir, sym_cfg.join("autostart")).unwrap();
    let mut sym_env = HashMap::new();
    sym_env.insert(
        "XDG_CONFIG_HOME".to_string(),
        sym_cfg.to_string_lossy().to_string(),
    );
    let sym_err = set_autostart(true, &["/x".to_string()], &sym_env, None).unwrap_err();
    assert!(sym_err.contains("regular directory"));

    // Portal response parsing & symlink screenshot rejection
    let token = "clipboardline_0123456789abcdef";
    let line = format!(
        "/org/freedesktop/portal/desktop/request/1_42/{}: org.freedesktop.portal.Request.Response (uint32 0, {{'uri': <'file:///tmp/shot%201.png'>}})",
        token
    );
    assert_eq!(
        parse_portal_response(&line, token),
        Some(PortalResponse {
            code: 0,
            uri: Some("file:///tmp/shot%201.png".to_string()),
        })
    );
    assert_eq!(parse_portal_response(&line, "other_token"), None);

    let real_shot = dir.path().join("shot.png");
    fs::write(&real_shot, b"png").unwrap();
    let sym_shot = dir.path().join("sym-shot.png");
    symlink(&real_shot, &sym_shot).unwrap();
    let sym_uri = Url::from_file_path(&sym_shot).unwrap().to_string();
    assert!(screenshot_path(Some(&PortalResponse {
        code: 0,
        uri: Some(sym_uri)
    }))
    .is_err());
}

#[test]
fn linux_media_probe_and_ffmpeg_arguments_hardening() {
    assert_eq!(INPUT_ARGS, ["-f", "mov", "-protocol_whitelist", "file"]);
    let probe = json!({
        "streams": [
            {
                "codec_type": "video",
                "codec_name": "h264",
                "width": 1920,
                "height": 1080,
                "bit_rate": "4500000",
                "side_data_list": [{ "rotation": -90 }]
            },
            {
                "codec_type": "audio",
                "codec_name": "aac"
            }
        ],
        "format": {
            "duration": "12.5"
        }
    });
    let desc = describe(&probe).unwrap();
    assert_eq!(desc.width, 1080);
    assert_eq!(desc.height, 1920);
    assert_eq!(desc.duration_ms, 12500.0);
    assert_eq!(desc.audio_tracks, 1);

    let avail = choose_encoders(" V..... libx264 H.264\n A..... aac AAC\n");
    assert_eq!(avail.videos, vec!["libx264".to_string()]);
    assert_eq!(avail.audios, vec!["aac".to_string()]);
    let attempts = encoder_attempts(&avail, 1);
    assert_eq!(attempts.len(), 1);
    let args = trim_arguments("/tmp/in.mp4", "/tmp/out.mp4", 1000.0, 4000.0, &attempts[0], &desc);
    assert!(args.windows(4).any(|w| w == ["-f", "mov", "-protocol_whitelist", "file"]));
}

#[test]
fn network_ssrf_guard_and_html_metadata_parsing() {
    for blocked in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.5.4",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "198.18.0.1",
        "::1",
        "::ffff:127.0.0.1",
        "fe80::1",
        "fc00::1",
        "2001:db8::1",
        "2001::1",
        "2002:7f00:1::1",
        "64:ff9b::127.0.0.1",
    ] {
        assert!(!public_address(blocked), "{} must be blocked", blocked);
    }
    assert!(public_address("93.184.216.34"));
    assert!(public_address("2606:2800:220:1:248:1893:25c8:1946"));

    let ok_res = public_url("https://example.com/page", Some(|_: &str| {
        Ok(vec!["93.184.216.34".parse::<IpAddr>().unwrap()])
    }));
    assert!(ok_res.is_ok());

    let rebind_res = public_url("https://example.com/page", Some(|_: &str| {
        Ok(vec![
            "93.184.216.34".parse::<IpAddr>().unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap(),
        ])
    }));
    assert!(rebind_res.is_err(), "any private IP in DNS response must block request");

    assert!(metadata_guard(&Url::parse("http://example.com").unwrap()).is_err());
    assert!(metadata_guard(&Url::parse("https://93.184.216.34/a").unwrap()).is_err());
    assert!(metadata_guard(&Url::parse("https://example.com./a").unwrap()).is_err());
    assert!(metadata_guard(&Url::parse("https://example.com/?access_token=secret").unwrap()).is_err());
    assert!(literal_destination(&Url::parse("https://example.com:8443/a").unwrap()).is_err());

    let html = b"<!doctype html><html><head>
        <meta charset=\"utf-8\">
        <meta property=\"og:title\" content=\"Merhaba &amp; Ho&#x15f;geldiniz \x07 \x00\">
        <meta property=\"og:image\" content=\"http://insecure.example.com/a.png\">
        <meta name=\"twitter:image\" content=\"/assets/card.png\">
    </head></html>";
    let decoded = decode_html(html, "text/html");
    let base = Url::parse("https://example.com/posts/1").unwrap();
    let meta = parse_html_metadata(&decoded, &base, "example.com");
    assert_eq!(meta.title, "Merhaba & Hoşgeldiniz");
    assert_eq!(
        meta.thumbnail,
        "https://example.com/assets/card.png",
        "insecure og:image is skipped in favor of valid HTTPS twitter:image"
    );
    assert_eq!(collapse(&decode_entities("A &mdash; B&#160;C")), "A — B C");
}

#[test]
fn export_drag_text_decoding_and_link_files() {
    assert_eq!(decode_text_file(b"\xef\xbb\xbfHello").unwrap(), "Hello");
    assert_eq!(
        decode_text_file(&[0xff, 0xfe, b'H', 0x00, b'i', 0x00]).unwrap(),
        "Hi"
    );
    assert_eq!(
        decode_text_file(&[0xfe, 0xff, 0x00, b'H', 0x00, b'i']).unwrap(),
        "Hi"
    );
    assert!(decode_text_file(&[0xff, 0xfe, b'H']).is_err());
    assert!(decode_text_file(&[0xff, 0xff, 0xff]).is_err());

    let lf = link_file("https://example.com/?q=a\\nb", "linux").unwrap();
    assert_eq!(lf.extension, ".desktop");
    assert!(lf.bytes.contains("\nURL=https://example.com/?q=a\\\\nb\n"));
    assert!(link_file("javascript:alert(1)", "linux").is_err());
    assert!(link_file("file:///etc/passwd", "linux").is_err());
    assert!(link_file("https://example.com/\x07", "linux").is_err());

    assert_eq!(sanitize_export_label("CON.txt", "text"), "_CON.txt");

    let dir = tempdir();
    let items = vec![
        json!({ "id": "00000000-0000-4000-8000-000000000001", "kind": "text", "text": "alpha", "title": "Note" }),
        json!({ "id": "00000000-0000-4000-8000-000000000002", "kind": "link", "url": "https://example.com/x", "title": "Site" }),
    ];
    let exported = export_copies(&items, dir.path(), "linux", |_| {
        Err("unused".to_string())
    })
    .unwrap();
    assert_eq!(exported.count, 2);
    assert!(dir.path().join("Note.txt").exists());
    assert!(dir.path().join("Site.desktop").exists());

    let mut drag = DragFiles::new(dir.path()).unwrap();
    let prepared = drag
        .prepare(&items, "linux", |_| Err("unused".to_string()))
        .unwrap();
    assert_eq!(prepared.0.len(), 2);
    drag.dispose();
}

#[test]
fn media_protocol_origin_and_range_validation() {
    assert_eq!(
        media_origin("GET", Some("http://tauri.localhost"), None),
        Some("http://tauri.localhost".to_string())
    );
    assert_eq!(media_origin("POST", Some("http://tauri.localhost"), None), None);
    assert_eq!(media_origin("GET", Some("https://evil.example"), None), None);

    let hash = "a".repeat(64);
    let png_path = PathBuf::from(format!("/tmp/{}.png", hash));
    assert!(media_generation_matches(
        Some(&format!("?v={}", hash)),
        None,
        &png_path
    ));
    assert!(!media_generation_matches(
        Some(&format!("?v={}", "b".repeat(64))),
        None,
        &png_path
    ));

    assert_eq!(
        byte_range(Some("bytes=0-3"), 10),
        Some(crate::media_protocol::ByteRange {
            start: 0,
            end: 3,
            partial: true
        })
    );
    assert_eq!(byte_range(Some("bytes=10-12"), 10), None);

    let dir = tempdir();
    let fpath = dir.path().join("video.mp4");
    fs::write(&fpath, b"0123456789").unwrap();
    let resp_206 =
        build_media_response(&fpath, "video/mp4", "http://tauri.localhost", Some("bytes=2-5"))
            .unwrap();
    assert_eq!(resp_206.status, 206);
    assert_eq!(resp_206.body, b"2345");

    let resp_416 =
        build_media_response(&fpath, "video/mp4", "http://tauri.localhost", Some("bytes=50-60"))
            .unwrap();
    assert_eq!(resp_416.status, 416);
}

#[derive(Default)]
struct FakeShortcutRegistry {
    map: HashMap<String, String>,
    deny: std::collections::HashSet<String>,
}

impl ShortcutRegistry for FakeShortcutRegistry {
    fn register(&mut self, key: &str, action: &str) -> bool {
        if self.deny.contains(key) || self.map.contains_key(key) {
            return false;
        }
        self.map.insert(key.to_string(), action.to_string());
        true
    }
    fn unregister(&mut self, key: &str) {
        self.map.remove(key);
    }
}

#[test]
fn shortcuts_support_atomic_swap_three_way_rotation_and_leases() {
    let mut sc = GlobalShortcuts::new(FakeShortcutRegistry::default(), Box::new(|_| {}));
    let mut initial = HashMap::new();
    initial.insert("toggle".to_string(), "Alt+V".to_string());
    initial.insert("search".to_string(), "Alt+F".to_string());
    initial.insert("library".to_string(), "Alt+L".to_string());
    assert!(sc.update(&initial).is_empty());

    // 2-way swap
    let mut swap2 = initial.clone();
    swap2.insert("toggle".to_string(), "Alt+F".to_string());
    swap2.insert("search".to_string(), "Alt+V".to_string());
    assert!(sc.update(&swap2).is_empty());
    assert_eq!(sc.registered.get("toggle").map(|s| s.as_str()), Some("Alt+F"));
    assert_eq!(sc.registered.get("search").map(|s| s.as_str()), Some("Alt+V"));

    // 3-way circular rotation
    let mut rot3 = swap2.clone();
    rot3.insert("toggle".to_string(), "Alt+V".to_string());
    rot3.insert("search".to_string(), "Alt+L".to_string());
    rot3.insert("library".to_string(), "Alt+F".to_string());
    assert!(sc.update(&rot3).is_empty());
    assert_eq!(sc.registered.get("library").map(|s| s.as_str()), Some("Alt+F"));

    // Suspend & resume leases
    let lease = sc.suspend().unwrap();
    assert!(sc.suspended());
    assert!(sc.registry.map.is_empty());
    sc.resume(lease);
    assert!(!sc.suspended());
    assert_eq!(sc.registry.map.len(), 3);
}

#[test]
fn ocr_queue_image_search_status_tray_and_motion_bridge() {
    let mut q = OcrQueue::new();
    for i in 0..300 {
        assert!(q.enqueue(&format!("id-{}", i), true, false));
    }
    assert_eq!(q.size(), 300, "OCR queue is not artificially capped at 200");
    q.discard_pending();
    assert_eq!(q.size(), 0);

    let mut vec512 = vec![0.0_f32; DIMENSIONS];
    vec512[0] = 3.0;
    vec512[1] = 4.0;
    let norm = normalized(&vec512).unwrap();
    assert!((norm[0] - 0.6).abs() < 1e-5);
    assert!((score(&norm, &[norm.clone()]).unwrap() - 1.0).abs() < 1e-5);

    let ranked = ranked_matches(&[
        ScoredItem {
            id: "a".to_string(),
            score: 0.35,
        },
        ScoredItem {
            id: "b".to_string(),
            score: 0.12,
        },
    ]);
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].id, "a");

    assert!(!image_regions(300, 200).unwrap().is_empty());
    let pixels = vec![128u8; 200 * 200 * 4];
    assert_eq!(image_tensors(&pixels, 200, 200).unwrap().len(), 1);

    assert_eq!(language_code(Some("system"), Some("tr_TR.UTF-8")), "tr");
    assert_eq!(
        translated("tr", "The line is empty", &[]),
        "Askılar boş"
    );
    let summary = status_summary(&[], &default_settings(), Some("1.7.1"), Some("en_US"));
    assert!(summary.highlight);
    assert!(!summary.dim);
    let bmp = badge_bitmap(&vec![0u8; 16 * 16 * 4], 16, "9+", false, true).unwrap();
    assert_eq!(bmp.len(), 16 * 16 * 4);

    let mut tracker = WindowBoundsTracker::new();
    let req = RectBounds {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    let real = RectBounds {
        x: 0,
        y: 28,
        width: 1920,
        height: 1052,
    };
    tracker.record(req, real, true);
    assert!(tracker.retains(real, req));

    let mut bridge = MotionBridge::new();
    let (tok, payload, mut rx, _) = bridge.prepare_request("open", &json!({ "after": 50 }));
    assert_eq!(payload["token"].as_u64(), Some(tok));
    assert!(bridge.acknowledge(tok, "open", true));
    assert_eq!(rx.try_recv().unwrap(), true);
}
