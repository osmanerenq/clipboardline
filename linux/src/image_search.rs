use crate::model::{ids, utf16_len};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tract_onnx::prelude::*;
use uuid::Uuid;

pub const DIMENSIONS: usize = 512;
pub const SIDE: usize = 256;
pub const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_CACHE_RECORDS: usize = 20_000;
pub const MODEL_CONFIG_JSON: &str = include_str!("../search-model-config.json");

fn key_pattern_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}$").unwrap())
}

fn bin_file_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}\.bin$").unwrap())
}

fn asset_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_.-]+$").unwrap())
}

fn clip_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+")
            .unwrap()
    })
}

pub fn normalized(values: &[f32]) -> Option<Vec<f32>> {
    if values.len() != DIMENSIONS {
        return None;
    }
    let mut sum = 0.0_f32;
    for &v in values {
        if !v.is_finite() {
            return None;
        }
        sum += v * v;
    }
    let norm = (sum as f64).sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return None;
    }
    Some(values.iter().map(|&v| ((v as f64) / norm) as f32).collect())
}

pub fn score(query: &[f32], vectors: &[Vec<f32>]) -> Option<f32> {
    if query.len() != DIMENSIONS
        || !query.iter().all(|v| v.is_finite())
        || vectors.is_empty()
        || vectors.len() > 4
    {
        return None;
    }
    let mut best = f32::NEG_INFINITY;
    for vector in vectors {
        if vector.len() != DIMENSIONS || !vector.iter().all(|v| v.is_finite()) {
            return None;
        }
        let mut sum = 0.0_f32;
        for i in 0..DIMENSIONS {
            sum += query[i] * vector[i];
        }
        if sum.is_finite() && sum > best {
            best = sum;
        }
    }
    if best.is_finite() {
        Some(best)
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoredItem {
    pub id: String,
    pub score: f64,
}

pub fn ranked_matches(values: &[ScoredItem]) -> Vec<ScoredItem> {
    let scores: Vec<ScoredItem> = values
        .iter()
        .filter(|v| v.score.is_finite())
        .cloned()
        .collect();
    let best = scores
        .iter()
        .map(|v| v.score)
        .fold(0.0_f64, |acc, s| acc.max(s));
    let mean = if !scores.is_empty() {
        scores.iter().map(|v| v.score).sum::<f64>() / (scores.len() as f64)
    } else {
        0.0
    };
    let spread = if !scores.is_empty() {
        let var = scores
            .iter()
            .map(|v| (v.score - mean).powi(2))
            .sum::<f64>()
            / (scores.len() as f64);
        var.sqrt().max(0.01)
    } else {
        1.0
    };
    let threshold = 0.22_f64.max(best - 0.045);
    let mut filtered: Vec<ScoredItem> = scores
        .iter()
        .filter(|v| {
            v.score >= threshold
                && (scores.len() < 8 || v.score >= 0.30 || (v.score - mean) / spread >= 1.25)
        })
        .cloned()
        .collect();
    filtered.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    filtered.truncate(200);
    filtered
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropView {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub fn image_regions(width: usize, height: usize) -> Result<Vec<Option<CropView>>, String> {
    if width < 1 || height < 1 || width > 1024 || height > 1024 {
        return Err("Invalid semantic image dimensions.".to_string());
    }
    let long = width.max(height) as f64;
    let short = width.min(height) as f64;
    if short < 120.0 || long / short > 4.0 {
        return Ok(Vec::new());
    }
    let mut views = vec![None];
    if long / short > 1.3 {
        let tiles = ((long / short).ceil() as usize).min(3);
        for i in 0..tiles {
            let offset = (long - short) * (i as f64) / ((tiles - 1) as f64);
            views.push(Some(CropView {
                x: if width >= height { offset } else { 0.0 },
                y: if width >= height { 0.0 } else { offset },
                width: short,
                height: short,
            }));
        }
    }
    Ok(views)
}

pub fn image_tensors(
    bitmap: &[u8],
    width: usize,
    height: usize,
) -> Result<Vec<Vec<f32>>, String> {
    if width == 0 || height == 0 || bitmap.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("Invalid semantic image pixels.".to_string());
    }
    let regions = image_regions(width, height)?;
    let mut out = Vec::with_capacity(regions.len());
    let w_f = width as f64;
    let h_f = height as f64;
    let side_f = SIDE as f64;

    for crop in regions {
        let mut output = vec![0.0_f32; 3 * SIDE * SIDE];
        let scale = side_f / w_f.max(h_f);
        let fit = if crop.is_some() {
            CropView {
                x: 0.0,
                y: 0.0,
                width: side_f,
                height: side_f,
            }
        } else {
            CropView {
                x: (side_f - w_f * scale) / 2.0,
                y: (side_f - h_f * scale) / 2.0,
                width: w_f * scale,
                height: h_f * scale,
            }
        };
        let region = crop.unwrap_or(CropView {
            x: 0.0,
            y: 0.0,
            width: w_f,
            height: h_f,
        });

        for y in 0..SIDE {
            for x in 0..SIDE {
                let at = y * SIDE + x;
                let xf = (x as f64) + 0.5;
                let yf = (y as f64) + 0.5;
                if xf < fit.x
                    || xf >= fit.x + fit.width
                    || yf < fit.y
                    || yf >= fit.y + fit.height
                {
                    let neutral = (128.0_f64 / 255.0_f64) as f32;
                    output[at] = neutral;
                    output[SIDE * SIDE + at] = neutral;
                    output[2 * SIDE * SIDE + at] = neutral;
                    continue;
                }
                let fx = (region.x + ((xf - fit.x) / fit.width) * region.width - 0.5)
                    .clamp(0.0, w_f - 1.0);
                let fy = (region.y + ((yf - fit.y) / fit.height) * region.height - 0.5)
                    .clamp(0.0, h_f - 1.0);
                let x0 = fx.floor() as usize;
                let y0 = fy.floor() as usize;
                let x1 = (x0 + 1).min(width - 1);
                let y1 = (y0 + 1).min(height - 1);
                let dx = fx - (x0 as f64);
                let dy = fy - (y0 as f64);

                let sample = |channel: usize| -> f64 {
                    let p00 = bitmap[(y0 * width + x0) * 4 + channel] as f64;
                    let p10 = bitmap[(y0 * width + x1) * 4 + channel] as f64;
                    let p01 = bitmap[(y1 * width + x0) * 4 + channel] as f64;
                    let p11 = bitmap[(y1 * width + x1) * 4 + channel] as f64;
                    let a = p00 * (1.0 - dx) + p10 * dx;
                    let b = p01 * (1.0 - dx) + p11 * dx;
                    a * (1.0 - dy) + b * dy
                };

                let alpha = sample(3) / 255.0;
                for channel in 0..3 {
                    let color = sample(2 - channel);
                    output[channel * SIDE * SIDE + at] =
                        ((color * alpha + 128.0 * (1.0 - alpha)) / 255.0) as f32;
                }
            }
        }
        out.push(output);
    }
    Ok(out)
}

pub struct ClipTokenizer {
    vocab: HashMap<String, i64>,
    ranks: HashMap<(String, String), usize>,
    byte_to_unicode: HashMap<u8, char>,
    cache: Mutex<HashMap<String, Vec<String>>>,
}

impl ClipTokenizer {
    pub fn new(vocab: HashMap<String, i64>, merges: &str) -> Result<Self, String> {
        if vocab.is_empty() {
            return Err("Invalid CLIP tokenizer assets.".to_string());
        }
        let mut ranks = HashMap::new();
        let merge_limit = 49152 - 256 - 2;
        for (idx, line) in merges.lines().skip(1).take(merge_limit).enumerate() {
            let mut parts = line.split_whitespace();
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                ranks.insert((a.to_string(), b.to_string()), idx);
            }
        }

        let mut bytes: Vec<u8> = Vec::with_capacity(256);
        for b in 33..=126 {
            bytes.push(b);
        }
        for b in 161..=172 {
            bytes.push(b);
        }
        for b in 174..=255 {
            bytes.push(b);
        }
        let mut characters: Vec<u32> = bytes.iter().map(|&b| b as u32).collect();
        let mut next = 256_u32;
        for b in 0..=255_u8 {
            if !bytes.contains(&b) {
                bytes.push(b);
                characters.push(next);
                next += 1;
            }
        }
        let mut byte_to_unicode = HashMap::with_capacity(256);
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(ch) = char::from_u32(characters[i]) {
                byte_to_unicode.insert(b, ch);
            }
        }

        Ok(Self {
            vocab,
            ranks,
            byte_to_unicode,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn from_files(vocab_path: &Path, merges_path: &Path) -> Result<Self, String> {
        let vocab_raw = fs::read_to_string(vocab_path).map_err(|e| e.to_string())?;
        let vocab: HashMap<String, i64> =
            serde_json::from_str(&vocab_raw).map_err(|_| "Invalid CLIP tokenizer assets.".to_string())?;
        let merges = fs::read_to_string(merges_path).map_err(|e| e.to_string())?;
        Self::new(vocab, &merges)
    }

    pub fn bpe(&self, word: &str) -> Vec<String> {
        if let Some(cached) = self.cache.lock().unwrap().get(word).cloned() {
            return cached;
        }
        let chars: Vec<char> = word.chars().collect();
        if chars.is_empty() {
            return Vec::new();
        }
        let mut parts: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
        if let Some(last) = parts.last_mut() {
            last.push_str("</w>");
        }

        while parts.len() > 1 {
            let mut best = usize::MAX;
            let mut selected = None;
            for i in 0..(parts.len() - 1) {
                if let Some(&rank) = self
                    .ranks
                    .get(&(parts[i].clone(), parts[i + 1].clone()))
                {
                    if rank < best {
                        best = rank;
                        selected = Some(i);
                    }
                }
            }
            let Some(sel) = selected else {
                break;
            };
            let first = parts[sel].clone();
            let second = parts[sel + 1].clone();
            let mut merged = Vec::with_capacity(parts.len());
            let mut i = 0;
            while i < parts.len() {
                if i + 1 < parts.len() && parts[i] == first && parts[i + 1] == second {
                    merged.push(format!("{}{}", first, second));
                    i += 2;
                } else {
                    merged.push(parts[i].clone());
                    i += 1;
                }
            }
            parts = merged;
        }

        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= 2048 {
            cache.clear();
        }
        cache.insert(word.to_string(), parts.clone());
        parts
    }

    pub fn encode(&self, text: &str) -> Vec<i64> {
        let truncated: String = text.chars().take(2048).collect();
        let lower = truncated.to_lowercase();
        let clean = lower.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut ids = vec![49406_i64];
        for m in clip_word_re().find_iter(&clean) {
            let token = m.as_str();
            let word: String = token
                .as_bytes()
                .iter()
                .filter_map(|b| self.byte_to_unicode.get(b).copied())
                .collect();
            for piece in self.bpe(&word) {
                if let Some(&val) = self.vocab.get(&piece) {
                    if (0..=49407).contains(&val) {
                        ids.push(val);
                    }
                }
            }
        }
        if ids.len() > 76 {
            ids.truncate(76);
        }
        ids.push(49407);
        ids
    }

    pub fn tensor(&self, text: &str) -> [i64; 77] {
        let mut out = [0_i64; 77];
        for (i, id) in self.encode(text).into_iter().enumerate().take(77) {
            out[i] = id;
        }
        out
    }
}

fn safe_asset_file(directory: &Path, name: &str, maximum: u64) -> Result<PathBuf, String> {
    let destination = directory.join(name);
    let meta = fs::symlink_metadata(&destination)
        .map_err(|_| "Offline image search assets are invalid.".to_string())?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.len() > maximum
        || meta.len() == 0
    {
        return Err("Offline image search assets are invalid.".to_string());
    }
    Ok(destination)
}

pub fn verify_models(model_directory: &Path, config: &Value) -> Result<(), String> {
    let dir_meta = fs::symlink_metadata(model_directory)
        .map_err(|_| "Offline image search assets are invalid.".to_string())?;
    if dir_meta.file_type().is_symlink() {
        return Err("Unsafe image search models.".to_string());
    }
    if config.get("schemaVersion").and_then(|v| v.as_i64()) != Some(1)
        || config.get("version").and_then(|v| v.as_str()).is_none()
    {
        return Err("Offline image model configuration is invalid.".to_string());
    }
    let Some(files) = config.get("files").and_then(|v| v.as_object()) else {
        return Err("Offline image model configuration is invalid.".to_string());
    };
    for (name, expected) in files {
        let bytes_expected = expected
            .get("bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let sha_expected = expected
            .get("sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !asset_name_re().is_match(name)
            || bytes_expected == 0
            || bytes_expected > 600 * 1024 * 1024
            || !key_pattern_re().is_match(sha_expected)
        {
            return Err("Invalid pinned model asset.".to_string());
        }
        let file = safe_asset_file(model_directory, name, bytes_expected)?;
        let stat = fs::metadata(&file).map_err(|e| e.to_string())?;
        if stat.len() != bytes_expected {
            return Err("Offline image model size differs.".to_string());
        }
        let mut f = File::open(&file).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        if hex::encode(hasher.finalize()) != sha_expected {
            return Err("Offline image model integrity differs.".to_string());
        }
    }
    Ok(())
}

pub fn valid_vectors(vectors: &[Vec<f32>]) -> bool {
    vectors.len() <= 4
        && vectors.iter().all(|vec| {
            if vec.len() != DIMENSIONS || !vec.iter().all(|v| v.is_finite()) {
                return false;
            }
            let sum_sq: f64 = vec.iter().map(|&v| (v as f64) * (v as f64)).sum();
            (sum_sq - 1.0).abs() < 0.02
        })
}

pub fn load_cached_vectors(
    cache_directory: &Path,
    model_id: &str,
    key: &str,
) -> Result<Option<Vec<Vec<f32>>>, String> {
    if !key_pattern_re().is_match(key) {
        return Err("Invalid image search identity.".to_string());
    }
    let Ok(file) = safe_asset_file(
        cache_directory,
        &format!("{}.bin", key),
        (76 + 4 * DIMENSIONS * 4) as u64,
    ) else {
        return Ok(None);
    };
    let Ok(record) = fs::read(&file) else {
        return Ok(None);
    };
    if record.len() < 76
        || &record[0..8] != b"CLIPIDX1"
        || hex::encode(&record[8..40]) != model_id
        || hex::encode(&record[40..72]) != key
    {
        return Ok(None);
    }
    let count = u32::from_le_bytes([record[72], record[73], record[74], record[75]]) as usize;
    if count > 4 || record.len() != 76 + count * DIMENSIONS * 4 {
        return Ok(None);
    }
    let mut vectors = Vec::with_capacity(count);
    for view in 0..count {
        let mut vec = Vec::with_capacity(DIMENSIONS);
        for index in 0..DIMENSIONS {
            let offset = 76 + (view * DIMENSIONS + index) * 4;
            let val = f32::from_le_bytes([
                record[offset],
                record[offset + 1],
                record[offset + 2],
                record[offset + 3],
            ]);
            vec.push(val);
        }
        vectors.push(vec);
    }
    if !valid_vectors(&vectors) {
        return Ok(None);
    }
    Ok(Some(vectors))
}

pub fn save_cached_vectors(
    cache_directory: &Path,
    model_id: &str,
    key: &str,
    vectors: &[Vec<f32>],
) -> Result<(), String> {
    if !key_pattern_re().is_match(key) {
        return Err("Invalid image search identity.".to_string());
    }
    let file = cache_directory.join(format!("{}.bin", key));
    if let Ok(meta) = fs::symlink_metadata(&file) {
        if meta.file_type().is_symlink() {
            return Err("Unsafe image search cache.".to_string());
        }
    }
    let model_bytes = hex::decode(model_id).map_err(|_| "Invalid model ID.".to_string())?;
    let key_bytes = hex::decode(key).map_err(|_| "Invalid key.".to_string())?;
    if model_bytes.len() != 32 || key_bytes.len() != 32 {
        return Err("Invalid cache header.".to_string());
    }
    let mut record = Vec::with_capacity(76 + vectors.len() * DIMENSIONS * 4);
    record.extend_from_slice(b"CLIPIDX1");
    record.extend_from_slice(&model_bytes);
    record.extend_from_slice(&key_bytes);
    record.extend_from_slice(&(vectors.len() as u32).to_le_bytes());
    for vec in vectors {
        for &val in vec {
            record.extend_from_slice(&val.to_le_bytes());
        }
    }

    let temporary = cache_directory.join(format!("{}.bin.tmp-{}", key, Uuid::new_v4()));
    let res = (|| -> Result<(), String> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        f.write_all(&record).map_err(|e| e.to_string())?;
        drop(f);
        fs::rename(&temporary, &file).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    res
}

pub fn prune_cache(cache_directory: &Path, keys: &[String]) -> Result<(), String> {
    if keys.len() > 100_000 || keys.iter().any(|k| !key_pattern_re().is_match(k)) {
        return Err("Invalid image index cleanup.".to_string());
    }
    let live: HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
    struct CacheRecord {
        file: PathBuf,
        size: u64,
        mtime: i64,
    }
    let mut records = Vec::new();
    if let Ok(entries) = fs::read_dir(cache_directory) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if !bin_file_re().is_match(&name) {
                continue;
            }
            let file = cache_directory.join(&name);
            let Ok(meta) = fs::symlink_metadata(&file) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                continue;
            }
            let key = &name[..64];
            if !live.contains(key) {
                let _ = fs::remove_file(&file);
            } else {
                records.push(CacheRecord {
                    file,
                    size: meta.len(),
                    mtime: meta.mtime(),
                });
            }
        }
    }
    records.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    let mut total_bytes: u64 = 0;
    for (idx, rec) in records.into_iter().enumerate() {
        total_bytes += rec.size;
        if idx >= MAX_CACHE_RECORDS || total_bytes > MAX_CACHE_BYTES {
            let _ = fs::remove_file(&rec.file);
        }
    }
    Ok(())
}

type RunnableModel = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

pub struct ImageSearchWorker {
    pub available: bool,
    pub model_directory: PathBuf,
    pub cache_directory: PathBuf,
    pub model_id: String,
    pub tokenizer: Option<ClipTokenizer>,
    image_session: Option<RunnableModel>,
    text_session: Option<RunnableModel>,
    query_cache: HashMap<String, Vec<f32>>,
    vector_cache: HashMap<String, Vec<Vec<f32>>>,
}

impl ImageSearchWorker {
    pub fn new(model_directory: PathBuf, cache_directory: PathBuf) -> Self {
        let mut worker = Self {
            available: false,
            model_directory,
            cache_directory,
            model_id: String::new(),
            tokenizer: None,
            image_session: None,
            text_session: None,
            query_cache: HashMap::new(),
            vector_cache: HashMap::new(),
        };
        let _ = worker.initialize();
        worker
    }

    fn initialize(&mut self) -> Result<(), String> {
        fs::create_dir_all(&self.cache_directory).map_err(|e| e.to_string())?;
        let _ = fs::set_permissions(&self.cache_directory, fs::Permissions::from_mode(0o700));
        let cache_meta =
            fs::symlink_metadata(&self.cache_directory).map_err(|e| e.to_string())?;
        if cache_meta.file_type().is_symlink() {
            return Err("Unsafe image search cache.".to_string());
        }
        let config: Value =
            serde_json::from_str(MODEL_CONFIG_JSON).map_err(|e| e.to_string())?;
        verify_models(&self.model_directory, &config)?;
        let src_file = safe_asset_file(&self.model_directory, "model-source.json", 1024 * 1024)?;
        let src_bytes = fs::read(&src_file).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        hasher.update(&src_bytes);
        self.model_id = hex::encode(hasher.finalize());
        let tokenizer = ClipTokenizer::from_files(
            &self.model_directory.join("clip-vocab.json"),
            &self.model_directory.join("clip-merges.txt"),
        )?;
        self.tokenizer = Some(tokenizer);
        self.available = true;
        Ok(())
    }

    fn ensure_sessions(&mut self) -> Result<(), String> {
        if self.image_session.is_some() && self.text_session.is_some() {
            return Ok(());
        }
        let img_path = safe_asset_file(
            &self.model_directory,
            "mobileclip-s2-image.onnx",
            400 * 1024 * 1024,
        )?;
        let txt_path = safe_asset_file(
            &self.model_directory,
            "mobileclip-s2-text.onnx",
            600 * 1024 * 1024,
        )?;

        let img_model = tract_onnx::onnx()
            .model_for_path(&img_path)
            .and_then(|m| {
                m.with_input_fact(0, f32::fact([1, 3, SIDE, SIDE]).into())?
                    .into_optimized()?
                    .into_runnable()
            })
            .map_err(|e| {
                self.available = false;
                format!("Failed to load image ONNX model: {}", e)
            })?;

        let txt_model = tract_onnx::onnx()
            .model_for_path(&txt_path)
            .and_then(|m| {
                m.with_input_fact(0, i64::fact([1, 77]).into())?
                    .into_optimized()?
                    .into_runnable()
            })
            .map_err(|e| {
                self.available = false;
                format!("Failed to load text ONNX model: {}", e)
            })?;

        self.image_session = Some(img_model);
        self.text_session = Some(txt_model);
        Ok(())
    }

    pub fn has(&mut self, key: &str) -> Result<bool, String> {
        if self.vector_cache.contains_key(key) {
            return Ok(true);
        }
        if let Some(vecs) = load_cached_vectors(&self.cache_directory, &self.model_id, key)? {
            if self.vector_cache.len() >= 256 {
                self.vector_cache.clear();
            }
            self.vector_cache.insert(key.to_string(), vecs);
            return Ok(true);
        }
        Ok(false)
    }

    pub fn index(
        &mut self,
        key: &str,
        bitmap: &[u8],
        width: usize,
        height: usize,
    ) -> Result<bool, String> {
        if !key_pattern_re().is_match(key) {
            return Err("Invalid image search identity.".to_string());
        }
        let views = image_tensors(bitmap, width, height)?;
        let mut embeddings = Vec::with_capacity(views.len());
        if !views.is_empty() {
            self.ensure_sessions()?;
            let session = self.image_session.as_ref().unwrap();
            for pixels in views {
                let tensor = tract_ndarray::Array4::from_shape_vec((1, 3, SIDE, SIDE), pixels)
                    .map_err(|e| e.to_string())?
                    .into_tensor();
                let outputs = session.run(tvec!(tensor.into())).map_err(|e| e.to_string())?;
                let slice = outputs[0]
                    .to_array_view::<f32>()
                    .map_err(|e| e.to_string())?;
                let raw: Vec<f32> = slice.iter().copied().collect();
                let norm = normalized(&raw)
                    .ok_or_else(|| "The offline model returned an invalid embedding.".to_string())?;
                embeddings.push(norm);
            }
        }
        save_cached_vectors(&self.cache_directory, &self.model_id, key, &embeddings)?;
        if self.vector_cache.len() >= 256 {
            self.vector_cache.clear();
        }
        self.vector_cache.insert(key.to_string(), embeddings);
        Ok(true)
    }

    pub fn search(
        &mut self,
        query: &str,
        entries: &[(String, String)],
    ) -> Result<(Vec<String>, Vec<ScoredItem>), String> {
        if utf16_len(query) > 2048
            || entries.len() > 20_000
            || entries
                .iter()
                .any(|(id, k)| id.is_empty() || !key_pattern_re().is_match(k))
        {
            return Err("Invalid image search query.".to_string());
        }
        self.ensure_sessions()?;
        let query_vec = if let Some(cached) = self.query_cache.get(query).cloned() {
            cached
        } else {
            let tokens = self
                .tokenizer
                .as_ref()
                .ok_or_else(|| "Offline image search is unavailable.".to_string())?
                .tensor(query);
            let tensor = tract_ndarray::Array2::from_shape_vec((1, 77), tokens.to_vec())
                .map_err(|e| e.to_string())?
                .into_tensor();
            let session = self.text_session.as_ref().unwrap();
            let outputs = session.run(tvec!(tensor.into())).map_err(|e| e.to_string())?;
            let slice = outputs[0]
                .to_array_view::<f32>()
                .map_err(|e| e.to_string())?;
            let raw: Vec<f32> = slice.iter().copied().collect();
            let norm = normalized(&raw)
                .ok_or_else(|| "The offline model returned an invalid embedding.".to_string())?;
            if self.query_cache.len() >= 16 {
                self.query_cache.clear();
            }
            self.query_cache.insert(query.to_string(), norm.clone());
            norm
        };

        let mut scores = Vec::new();
        for (id, key) in entries {
            let vecs_opt = if let Some(v) = self.vector_cache.get(key).cloned() {
                Some(v)
            } else if let Ok(Some(v)) =
                load_cached_vectors(&self.cache_directory, &self.model_id, key)
            {
                if self.vector_cache.len() >= 256 {
                    self.vector_cache.clear();
                }
                self.vector_cache.insert(key.clone(), v.clone());
                Some(v)
            } else {
                None
            };
            if let Some(vecs) = vecs_opt {
                if !vecs.is_empty() {
                    if let Some(s) = score(&query_vec, &vecs) {
                        scores.push(ScoredItem {
                            id: id.clone(),
                            score: s as f64,
                        });
                    }
                }
            }
        }
        let ranked = ranked_matches(&scores);
        let ranked_ids = ranked.into_iter().map(|r| r.id).collect();
        Ok((ranked_ids, scores))
    }
}

pub fn decode_png_to_bgra_384(image_path: &Path) -> Result< Option<(Vec<u8>, usize, usize)>, String> {
    let meta = fs::symlink_metadata(image_path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.len() > 64 * 1024 * 1024
    {
        return Ok(None);
    }
    let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
    if !crate::linux_clipboard::decodable_png(&bytes) {
        return Ok(None);
    }
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    if w < 1 || h < 1 || (w as u64) * (h as u64) > 80_000_000 {
        return Ok(None);
    }
    let scaled = if w.max(h) > 384 {
        let (nw, nh) = if w >= h {
            (384, ((h as u64 * 384) / w as u64).max(1) as u32)
        } else {
            (((w as u64 * 384) / h as u64).max(1) as u32, 384)
        };
        img.resize_exact(nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = scaled.to_rgba8();
    let (sw, sh) = (rgba.width() as usize, rgba.height() as usize);
    let mut bgra = rgba.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Some((bgra, sw, sh)))
}

pub fn item_image_key(item: &Value) -> Option<String> {
    if item.get("kind").and_then(|v| v.as_str()) != Some("image")
        || item.get("trashedAt").map(|v| !v.is_null()).unwrap_or(false)
    {
        return None;
    }
    let file = item
        .get("renderedFile")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| item.get("imageFile").and_then(|v| v.as_str()))?;
    if file.len() == 68 && file.ends_with(".png") && key_pattern_re().is_match(&file[..64]) {
        Some(file[..64].to_string())
    } else {
        None
    }
}

pub fn search_images_with_worker(
    worker: &Arc<Mutex<ImageSearchWorker>>,
    items: &[Value],
    query: &str,
    eligible_ids_val: Option<&Value>,
) -> Result<Value, String> {
    if utf16_len(query) > 2048 || query.contains('\0') {
        return Err("Search images with up to 2,048 characters.".to_string());
    }
    let Some(raw_ids) = eligible_ids_val.and_then(|v| v.as_array()) else {
        return Err("Invalid image search scope.".to_string());
    };
    if raw_ids.len() > 20_000 {
        return Err("Invalid image search scope.".to_string());
    }
    let mut eligible = HashSet::new();
    for chunk in raw_ids.chunks(10_000) {
        for id in ids(Some(&Value::Array(chunk.to_vec()))).map_err(|e| e.message)? {
            eligible.insert(id);
        }
    }
    let phrase = query.trim();
    let mut w = worker.lock().unwrap();
    if phrase.is_empty() {
        return Ok(json!({
            "ids": [],
            "scores": [],
            "available": w.available,
            "indexing": false
        }));
    }
    if !w.available {
        return Ok(json!({
            "ids": [],
            "scores": [],
            "available": false,
            "indexing": false
        }));
    }
    let entries: Vec<(String, String)> = items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(|v| v.as_str())?;
            if !eligible.contains(id) {
                return None;
            }
            let key = item_image_key(item)?;
            Some((id.to_string(), key))
        })
        .collect();

    let (ranked_ids, scores) = w.search(phrase, &entries)?;
    Ok(json!({
        "ids": ranked_ids,
        "scores": scores,
        "available": true,
        "indexing": false
    }))
}
