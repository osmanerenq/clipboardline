use encoding_rs::Encoding;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use url::Url;

fn local_host_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(^localhost$|\.(local|internal|home|localhost|test|invalid)$)").unwrap()
    })
}

fn sensitive_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(xamz|xgoog|oauth)").unwrap())
}

fn sensitive_keys() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "token",
            "accesstoken",
            "refreshtoken",
            "idtoken",
            "apikey",
            "auth",
            "authorization",
            "authentication",
            "authtoken",
            "code",
            "signature",
            "sig",
            "session",
            "sessionid",
            "state",
            "password",
            "secret",
            "clientsecret",
            "credential",
            "credentials",
            "accesskeyid",
            "key",
            "keypairid",
            "policy",
            "verifier",
            "jwt",
            "bearer",
        ]
        .into_iter()
        .collect()
    })
}

fn named_entities() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        [
            ("amp", "&"),
            ("quot", "\""),
            ("lt", "<"),
            ("gt", ">"),
            ("apos", "'"),
            ("nbsp", " "),
            ("ccedil", "ç"),
            ("Ccedil", "Ç"),
            ("ouml", "ö"),
            ("Ouml", "Ö"),
            ("uuml", "ü"),
            ("Uuml", "Ü"),
            ("gbreve", "ğ"),
            ("Gbreve", "Ğ"),
            ("scedil", "ş"),
            ("Scedil", "Ş"),
            ("inodot", "ı"),
            ("Idot", "İ"),
            ("acirc", "â"),
            ("icirc", "î"),
            ("ucirc", "û"),
            ("auml", "ä"),
            ("Auml", "Ä"),
            ("szlig", "ß"),
            ("eacute", "é"),
            ("Eacute", "É"),
            ("egrave", "è"),
            ("agrave", "à"),
            ("aacute", "á"),
            ("iacute", "í"),
            ("oacute", "ó"),
            ("uacute", "ú"),
            ("ntilde", "ñ"),
            ("ndash", "–"),
            ("mdash", "—"),
            ("hellip", "…"),
            ("lsquo", "‘"),
            ("rsquo", "’"),
            ("ldquo", "“"),
            ("rdquo", "”"),
            ("laquo", "«"),
            ("raquo", "»"),
            ("bull", "•"),
            ("middot", "·"),
            ("copy", "©"),
            ("reg", "®"),
            ("trade", "™"),
            ("euro", "€"),
            ("pound", "£"),
        ]
        .into_iter()
        .collect()
    })
}

fn entity_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"&(?:#(\d{1,7})|#[xX]([0-9a-fA-F]{1,6})|([A-Za-z]{2,8}));").unwrap()
    })
}

fn meta_charset_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)<meta[^>]*[\s"';]charset\s*=\s*["']?([\w.:-]+)"#).unwrap())
}

fn header_charset_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)(?:^|;)\s*charset\s*=\s*["']?([\w.:-]+)"#).unwrap())
}

fn xml_encoding_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)^\s*<\?xml[^>]*\sencoding\s*=\s*["']([\w.:-]+)"#).unwrap())
}

fn meta_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<meta\b([^>]{0,4096})>").unwrap())
}

fn attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"([A-Za-z_:][A-Za-z0-9_:.-]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))"#)
            .unwrap()
    })
}

fn title_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<title[^>]*>([\s\S]{0,2000}?)</title>").unwrap())
}

fn html_tags_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]*>").unwrap())
}

pub fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}

pub fn public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_ipv4(v4);
    }
    let words = ip.segments();
    (words[0] & 0xe000) == 0x2000
        && !(words[0] == 0x2001 && ((words[1] & 0xfe00) == 0 || words[1] == 0x0db8))
        && words[0] != 0x2002
        && !(words[0] == 0x3fff && (words[1] & 0xf000) == 0)
}

pub fn public_address(address: &str) -> bool {
    let trimmed = address.trim_start_matches('[').trim_end_matches(']');
    if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("::ffff:") {
        if let Ok(v4) = rest.parse::<Ipv4Addr>() {
            return public_ipv4(v4);
        }
    }
    match trimmed.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => public_ipv4(v4),
        Ok(IpAddr::V6(v6)) => public_ipv6(v6),
        Err(_) => false,
    }
}

pub fn literal_destination(url: &Url) -> Result<String, String> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("Only public HTTP(S) destinations are allowed".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Only public HTTP(S) destinations are allowed".to_string());
    }
    if let Some(port) = url.port() {
        if port != 80 && port != 443 {
            return Err("Only public HTTP(S) destinations are allowed".to_string());
        }
    }
    let Some(raw_host) = url.host_str() else {
        return Err("Only public HTTP(S) destinations are allowed".to_string());
    };
    let host = raw_host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if local_host_suffix_re().is_match(&host) {
        return Err("Only public HTTP(S) destinations are allowed".to_string());
    }
    if host.parse::<IpAddr>().is_ok() && !public_address(&host) {
        return Err("Private network destinations are blocked".to_string());
    }
    Ok(host)
}

pub fn public_url<R>(input: &str, resolver: Option<R>) -> Result<(Url, IpAddr), String>
where
    R: Fn(&str) -> Result<Vec<IpAddr>, String>,
{
    let url =
        Url::parse(input).map_err(|_| "Only public HTTP(S) destinations are allowed".to_string())?;
    let host = literal_destination(&url)?;
    let addresses: Vec<IpAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else if let Some(res_fn) = resolver {
        res_fn(&host)?
    } else {
        let port = url.port_or_known_default().unwrap_or(443);
        (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|_| "Private network destinations are blocked".to_string())?
            .map(|sa| sa.ip())
            .collect()
    };

    if addresses.is_empty()
        || addresses
            .iter()
            .any(|ip| !public_address(&ip.to_string()))
    {
        return Err("Private network destinations are blocked".to_string());
    }
    Ok((url, addresses[0]))
}

pub fn metadata_guard(url: &Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err("Automatic link metadata requires HTTPS.".to_string());
    }
    let Some(raw_host) = url.host_str() else {
        return Err("Automatic link metadata needs a named host.".to_string());
    };
    let host = raw_host.trim_start_matches('[').trim_end_matches(']');
    if host.parse::<IpAddr>().is_ok() || raw_host.ends_with('.') {
        return Err("Automatic link metadata needs a named host.".to_string());
    }
    for (key, _) in url.query_pairs() {
        let normalized: String = key
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if sensitive_prefix_re().is_match(&normalized)
            || sensitive_keys().contains(normalized.as_str())
        {
            return Err("Sensitive link parameters are not fetched automatically.".to_string());
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub bytes: Vec<u8>,
    pub url: String,
    pub content_type: String,
}

pub async fn bounded_fetch(
    input: &str,
    maximum: usize,
    redirects: usize,
    timeout_ms: u64,
    deadline_ms: u64,
    use_metadata_guard: bool,
) -> Result<FetchResponse, String> {
    let start = Instant::now();
    let total_deadline = Duration::from_millis(deadline_ms);
    let mut current_url = input.to_string();
    let mut remaining_redirects = redirects;

    loop {
        let parsed = Url::parse(&current_url)
            .map_err(|_| "Only public HTTP(S) destinations are allowed".to_string())?;
        if use_metadata_guard {
            metadata_guard(&parsed)?;
        }
        let elapsed = start.elapsed();
        if elapsed >= total_deadline {
            return Err("Network request timed out".to_string());
        }
        let left = total_deadline - elapsed;

        let url_for_dns = current_url.clone();
        let (validated_url, pinned_ip) = tokio::time::timeout(
            left,
            tokio::task::spawn_blocking(move || {
                public_url::<fn(&str) -> Result<Vec<IpAddr>, String>>(&url_for_dns, None)
            }),
        )
        .await
        .map_err(|_| "Network request timed out".to_string())?
        .map_err(|_| "Network request failed".to_string())??;

        let elapsed = start.elapsed();
        if elapsed >= total_deadline {
            return Err("Network request timed out".to_string());
        }
        let req_timeout = (total_deadline - elapsed).min(Duration::from_millis(timeout_ms));

        let host = validated_url.host_str().unwrap_or("");
        let port = validated_url.port_or_known_default().unwrap_or(443);
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(req_timeout)
            .user_agent("ClipBoardLine/1.7.0");
        if host.parse::<IpAddr>().is_err() {
            builder = builder.resolve(host, SocketAddr::new(pinned_ip, port));
        }
        let client = builder.build().map_err(|e| e.to_string())?;
        let mut resp = client
            .get(validated_url.as_str())
            .header("Accept", "*/*")
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "Network request timed out".to_string()
                } else {
                    e.to_string()
                }
            })?;

        let status = resp.status().as_u16();
        if [301, 302, 303, 307, 308].contains(&status) {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok());
            if remaining_redirects == 0 || loc.is_none() {
                return Err("Too many redirects".to_string());
            }
            let next = validated_url
                .join(loc.unwrap())
                .map_err(|_| "Invalid redirect location".to_string())?;
            current_url = next.to_string();
            remaining_redirects -= 1;
            continue;
        }

        let content_len = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        if status != 200 || content_len > maximum {
            return Err("Remote response is unavailable or too large".to_string());
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| {
            if e.is_timeout() {
                "Network request timed out".to_string()
            } else {
                e.to_string()
            }
        })? {
            if start.elapsed() >= total_deadline {
                return Err("Network request timed out".to_string());
            }
            if bytes.len() + chunk.len() > maximum {
                return Err("Response exceeds size limit".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }

        return Ok(FetchResponse {
            bytes,
            url: validated_url.to_string(),
            content_type,
        });
    }
}

pub fn decode_entities(value: &str) -> String {
    entity_re()
        .replace_all(value, |caps: &regex::Captures| {
            if let Some(name_match) = caps.get(3) {
                if let Some(&replacement) = named_entities().get(name_match.as_str()) {
                    return replacement.to_string();
                }
                return caps[0].to_string();
            }
            let code_opt = if let Some(dec) = caps.get(1) {
                dec.as_str().parse::<u32>().ok()
            } else if let Some(hex_str) = caps.get(2) {
                u32::from_str_radix(hex_str.as_str(), 16).ok()
            } else {
                None
            };
            if let Some(code) = code_opt {
                if code > 0x1f
                    && code <= 0x10ffff
                    && !(0x7f..=0x9f).contains(&code)
                    && !(0xd800..=0xdfff).contains(&code)
                {
                    if let Some(ch) = char::from_u32(code) {
                        return ch.to_string();
                    }
                }
            }
            caps[0].to_string()
        })
        .into_owned()
}

pub fn decode_html(bytes: &[u8], content_type: &str) -> String {
    let head_len = bytes.len().min(2048);
    let head: String = bytes[..head_len].iter().map(|&b| b as char).collect();

    let mut candidates: Vec<String> = Vec::new();
    if bytes.len() >= 3 && bytes[0] == 0xef && bytes[1] == 0xbb && bytes[2] == 0xbf {
        candidates.push("utf-8".to_string());
    }
    if let Some(caps) = header_charset_re().captures(content_type) {
        if let Some(m) = caps.get(1) {
            candidates.push(m.as_str().to_string());
        }
    }
    if content_type.to_ascii_lowercase().contains("application/xhtml+xml") {
        if let Some(caps) = xml_encoding_re().captures(&head) {
            if let Some(m) = caps.get(1) {
                candidates.push(m.as_str().to_string());
            }
        }
    }
    for caps in meta_charset_re().captures_iter(&head) {
        if let Some(m) = caps.get(1) {
            let label = m.as_str();
            if label.to_ascii_lowercase().starts_with("utf-16") {
                candidates.push("utf-8".to_string());
            } else {
                candidates.push(label.to_string());
            }
        }
    }

    for label in candidates {
        if let Some(enc) = Encoding::for_label(label.as_bytes()) {
            let (cow, _, _) = enc.decode(bytes);
            return cow.into_owned();
        }
    }

    if let Ok(utf8) = std::str::from_utf8(bytes) {
        utf8.to_string()
    } else {
        let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
        cow.into_owned()
    }
}

pub fn collapse(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let mut prev_space = false;
    for ch in value.chars() {
        let code = ch as u32;
        if code <= 0x08
            || (0x0e..=0x1f).contains(&code)
            || (0x7f..=0x84).contains(&code)
            || (0x86..=0x9f).contains(&code)
        {
            continue;
        }
        if ch.is_whitespace() || ch == '\u{0085}' {
            if !prev_space && !cleaned.is_empty() {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(ch);
            prev_space = false;
        }
    }
    cleaned.trim().to_string()
}

pub fn extract_meta(html: &str, target_name: &str) -> String {
    let target_lower = target_name.to_ascii_lowercase();
    for caps in meta_tag_re().captures_iter(html) {
        let Some(attrs_str) = caps.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let mut prop_val = None;
        let mut name_val = None;
        let mut content_val = None;
        for acaps in attr_re().captures_iter(attrs_str) {
            let key = acaps.get(1).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default();
            let val = acaps
                .get(2)
                .or_else(|| acaps.get(3))
                .or_else(|| acaps.get(4))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            match key.as_str() {
                "property" => prop_val = Some(val),
                "name" => name_val = Some(val),
                "content" => content_val = Some(val),
                _ => {}
            }
        }
        let matches = prop_val
            .as_ref()
            .map(|p| p.to_ascii_lowercase() == target_lower)
            .unwrap_or(false)
            || name_val
                .as_ref()
                .map(|n| n.to_ascii_lowercase() == target_lower)
                .unwrap_or(false);
        if matches {
            return decode_entities(&content_val.unwrap_or_default());
        }
    }
    String::new()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkMetadataResult {
    pub title: String,
    pub author: String,
    pub thumbnail: String,
}

pub fn parse_html_metadata(html: &str, response_url: &Url, fallback_host: &str) -> LinkMetadataResult {
    let og_title = collapse(&extract_meta(html, "og:title"));
    let tw_title = collapse(&extract_meta(html, "twitter:title"));
    let tag_title = title_tag_re()
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| {
            let stripped = html_tags_re().replace_all(m.as_str(), "");
            collapse(&decode_entities(&stripped))
        })
        .unwrap_or_default();

    let raw_title = if !og_title.is_empty() {
        og_title
    } else if !tw_title.is_empty() {
        tw_title
    } else if !tag_title.is_empty() {
        tag_title
    } else {
        fallback_host.to_string()
    };
    let title: String = raw_title.chars().take(500).collect();

    let mut thumbnail = String::new();
    for meta_key in ["og:image:secure_url", "og:image", "twitter:image"] {
        let val = extract_meta(html, meta_key);
        if val.trim().is_empty() {
            continue;
        }
        if let Ok(img_url) = response_url.join(val.trim()) {
            if metadata_guard(&img_url).is_ok() && literal_destination(&img_url).is_ok() {
                thumbnail = img_url.to_string();
                break;
            }
        }
    }

    LinkMetadataResult {
        title,
        author: String::new(),
        thumbnail,
    }
}

pub async fn link_metadata(input: &str) -> Result<LinkMetadataResult, String> {
    let url = Url::parse(input).map_err(|e| e.to_string())?;
    metadata_guard(&url)?;
    let hostname = url.host_str().unwrap_or("").to_string();
    let is_youtube = matches!(
        hostname.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    );

    if is_youtube {
        let mut video_url = url.clone();
        if video_url.host_str() == Some("music.youtube.com") {
            let _ = video_url.set_host(Some("www.youtube.com"));
        }
        let encoded_target: String =
            url::form_urlencoded::byte_serialize(video_url.as_str().as_bytes()).collect();
        let oembed_url = format!(
            "https://www.youtube.com/oembed?url={}&format=json",
            encoded_target
        );
        let resp = bounded_fetch(&oembed_url, 64 * 1024, 3, 8000, 20000, true).await?;
        let data: Value = serde_json::from_slice(&resp.bytes).map_err(|e| e.to_string())?;
        let Some(obj) = data.as_object() else {
            return Ok(LinkMetadataResult {
                title: hostname.to_string(),
                author: String::new(),
                thumbnail: String::new(),
            });
        };
        let raw_title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let title: String = if raw_title.is_empty() {
            hostname.to_string()
        } else {
            raw_title.chars().take(500).collect()
        };
        let author: String = obj
            .get("author_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        let thumbnail = obj
            .get("thumbnail_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(LinkMetadataResult {
            title,
            author,
            thumbnail,
        });
    }

    let resp = bounded_fetch(url.as_str(), 512 * 1024, 3, 8000, 20000, true).await?;
    let ct_lower = resp.content_type.to_ascii_lowercase();
    if !ct_lower.contains("text/html") && !ct_lower.contains("application/xhtml") {
        return Ok(LinkMetadataResult {
            title: hostname.to_string(),
            author: String::new(),
            thumbnail: String::new(),
        });
    }
    let html = decode_html(&resp.bytes, &resp.content_type);
    let final_url = Url::parse(&resp.url).unwrap_or(url);
    Ok(parse_html_metadata(&html, &final_url, &hostname))
}
