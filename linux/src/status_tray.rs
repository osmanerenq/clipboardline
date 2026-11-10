use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

const LOCALES_JS: &str = include_str!("../frontend/locales.js");

fn catalogs() -> &'static HashMap<String, HashMap<String, String>> {
    static MAP: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();
    MAP.get_or_init(|| {
        let start = LOCALES_JS
            .find("Object.freeze(")
            .map(|i| i + "Object.freeze(".len())
            .unwrap_or(0);
        let end = LOCALES_JS.rfind(");if(typeof module").unwrap_or(LOCALES_JS.len());
        let json_slice = &LOCALES_JS[start..end];
        serde_json::from_str(json_slice).unwrap_or_default()
    })
}

pub fn language_code(value: Option<&str>, system_locale: Option<&str>) -> String {
    let sys = system_locale.filter(|s| !s.is_empty()).unwrap_or("en");
    let raw = match value {
        Some(v) if !v.is_empty() && v != "system" => v,
        _ => sys,
    };
    let lower = raw.to_ascii_lowercase();
    if lower == "pt" || lower.starts_with("pt-") || lower.starts_with("pt_") {
        return "pt-BR".to_string();
    }
    if lower == "zh" || lower.starts_with("zh-") || lower.starts_with("zh_") {
        return "zh-Hans".to_string();
    }
    let lang = lower
        .split(['-', '_'])
        .next()
        .unwrap_or("en")
        .to_string();
    if catalogs().contains_key(&lang) {
        lang
    } else {
        "en".to_string()
    }
}

pub fn translated(language: &str, key: &str, values: &[String]) -> String {
    let cat = catalogs();
    let template = cat
        .get(language)
        .and_then(|m| m.get(key))
        .or_else(|| cat.get("en").and_then(|m| m.get(key)))
        .map(|s| s.as_str())
        .unwrap_or(key);

    let mut out = String::with_capacity(template.len() + 16);
    let mut chars = template.chars().peekable();
    let mut idx = 0;
    while let Some(ch) = chars.next() {
        if ch == '%' {
            if let Some(&next) = chars.peek() {
                if next == '@' || next == 'd' {
                    chars.next();
                    let val = values.get(idx).map(|s| s.as_str()).unwrap_or("");
                    idx += 1;
                    out.push_str(val);
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSummary {
    pub images: usize,
    pub texts: usize,
    pub links: usize,
    pub count: usize,
    pub badge: String,
    pub language: String,
    pub description: String,
    pub tooltip: String,
    #[serde(rename = "updateVersion")]
    pub update_version: Option<String>,
    pub dim: bool,
    pub highlight: bool,
    #[serde(rename = "iconKey")]
    pub icon_key: String,
    #[serde(rename = "menuKey")]
    pub menu_key: String,
    pub key: String,
}

pub fn status_summary(
    items: &[Value],
    settings: &Value,
    update_version: Option<&str>,
    system_locale: Option<&str>,
) -> StatusSummary {
    let visible_arr = settings
        .get("visibleKinds")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty());
    let kinds: HashSet<&str> = if let Some(arr) = visible_arr {
        arr.iter().filter_map(|v| v.as_str()).collect()
    } else {
        ["image", "text", "link"].into_iter().collect()
    };

    let mut images = 0_usize;
    let mut texts = 0_usize;
    let mut links = 0_usize;
    for item in items {
        let trashed = item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false);
        let hidden = item
            .get("hiddenFromShelf")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if !trashed && !hidden && kinds.contains(kind) {
            match kind {
                "image" => images += 1,
                "text" => texts += 1,
                "link" => links += 1,
                _ => {}
            }
        }
    }

    let count = images + texts + links;
    let badge = if count > 9 {
        "9+".to_string()
    } else {
        count.to_string()
    };
    let pref_lang = settings.get("language").and_then(|v| v.as_str());
    let language = language_code(pref_lang, system_locale);
    let version = update_version
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let description = if count > 0 {
        translated(
            &language,
            "%d images · %d texts · %d links on the line",
            &[images.to_string(), texts.to_string(), links.to_string()],
        )
    } else {
        translated(&language, "The line is empty", &[])
    };

    let update_line = version.as_ref().map(|ver| {
        let short_ver = if ver.chars().count() > 28 {
            format!("{}…", ver.chars().take(27).collect::<String>())
        } else {
            ver.clone()
        };
        format!(
            "{} · {}",
            translated(&language, "Update available", &[]),
            short_ver
        )
    });

    let mut tooltip_lines = vec!["ClipBoardLine".to_string(), description.clone()];
    if let Some(u) = update_line {
        tooltip_lines.push(u);
    }
    let tooltip: String = tooltip_lines.join("\n").chars().take(127).collect();
    let dim = count == 0 && version.is_none();
    let highlight = version.is_some();
    let icon_key = format!(
        "{}:{}:{}",
        badge,
        if dim { "dim" } else { "normal" },
        highlight
    );
    let menu_key = json!([language, version]).to_string();
    let key = json!([images, texts, links, language, version]).to_string();

    StatusSummary {
        images,
        texts,
        links,
        count,
        badge,
        language,
        description,
        tooltip,
        update_version: version,
        dim,
        highlight,
        icon_key,
        menu_key,
        key,
    }
}

fn glyph_rows(ch: char) -> Option<[&'static str; 5]> {
    match ch {
        '0' => Some(["111", "101", "101", "101", "111"]),
        '1' => Some(["010", "110", "010", "010", "111"]),
        '2' => Some(["111", "001", "111", "100", "111"]),
        '3' => Some(["111", "001", "111", "001", "111"]),
        '4' => Some(["101", "101", "111", "001", "001"]),
        '5' => Some(["111", "100", "111", "001", "111"]),
        '6' => Some(["111", "100", "111", "101", "111"]),
        '7' => Some(["111", "001", "010", "010", "010"]),
        '8' => Some(["111", "101", "111", "101", "111"]),
        '9' => Some(["111", "101", "111", "001", "111"]),
        '+' => Some(["000", "010", "111", "010", "000"]),
        _ => None,
    }
}

pub fn badge_bitmap(
    source: &[u8],
    size: usize,
    badge: &str,
    dim: bool,
    highlight: bool,
) -> Result<Vec<u8>, String> {
    let valid_badge =
        (badge.len() == 1 && badge.chars().next().and_then(glyph_rows).is_some()) || badge == "9+";
    if source.len() != size * size * 4 || ![16, 32, 48].contains(&size) || !valid_badge {
        return Err("Invalid tray artwork or badge.".to_string());
    }
    let mut buffer = source.to_vec();
    let scale = size / 16;
    let width = (if badge.len() == 2 { 9 } else { 7 }) * scale;
    let height = 7 * scale;
    let left = size - width;
    let top = size - height;
    let radius = (3 * scale) as f64;

    let paint = |buf: &mut [u8], x: usize, y: usize, color: [u8; 3]| {
        let at = (y * size + x) * 4;
        buf[at] = color[2];
        buf[at + 1] = color[1];
        buf[at + 2] = color[0];
        buf[at + 3] = 255;
    };

    let fill = if highlight {
        [10, 132, 255]
    } else {
        [246, 250, 255]
    };
    let ink = if highlight {
        [255, 255, 255]
    } else {
        [16, 35, 59]
    };

    for y in top..size {
        for x in left..size {
            let xf = (x as f64) + 0.5;
            let yf = (y as f64) + 0.5;
            let cx = xf.clamp((left as f64) + radius, (size as f64) - radius);
            let cy = yf.clamp((top as f64) + radius, (size as f64) - radius);
            if (xf - cx).powi(2) + (yf - cy).powi(2) <= radius.powi(2) {
                paint(&mut buffer, x, y, fill);
            }
        }
    }

    let text_width = (badge.len() * 3 + badge.len() - 1) * scale;
    let start_x = left + (width - text_width) / 2;
    let start_y = top + scale;

    for (index, ch) in badge.chars().enumerate() {
        if let Some(rows) = glyph_rows(ch) {
            for (y, row) in rows.iter().enumerate() {
                for (x, bit) in row.chars().enumerate() {
                    if bit == '1' {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                paint(
                                    &mut buffer,
                                    start_x + index * 4 * scale + x * scale + dx,
                                    start_y + y * scale + dy,
                                    ink,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if dim {
        for byte in &mut buffer {
            *byte = ((*byte as f64) * 0.45).round() as u8;
        }
    }
    Ok(buffer)
}

pub fn render_tray_rgba(base_png: &[u8], summary: &StatusSummary) -> Result<(Vec<u8>, u32, u32), String> {
    let img = image::load_from_memory_with_format(base_png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let resized = img
        .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let mut bgra = resized.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let badged_bgra = badge_bitmap(&bgra, 32, &summary.badge, summary.dim, summary.highlight)?;
    let mut rgba = badged_bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok((rgba, 32, 32))
}
