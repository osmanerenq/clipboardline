use crate::linux_clipboard::{decodable_png, read_process};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::Cursor;
use std::path::Path;

pub struct OcrQueue {
    items: VecDeque<String>,
    pending: HashSet<String>,
}

impl OcrQueue {
    pub fn new() -> Self {
        Self {
            items: VecDeque::new(),
            pending: HashSet::new(),
        }
    }

    pub fn enqueue(&mut self, id: &str, enabled: bool, closing: bool) -> bool {
        if closing || !enabled || self.pending.contains(id) {
            return false;
        }
        self.items.push_back(id.to_string());
        self.pending.insert(id.to_string());
        true
    }

    pub fn dequeue(&mut self) -> Option<String> {
        let id = self.items.pop_front()?;
        self.pending.remove(&id);
        Some(id)
    }

    pub fn discard_pending(&mut self) {
        self.items.clear();
        self.pending.clear();
    }

    pub fn size(&self) -> usize {
        self.items.len()
    }
}

impl Default for OcrQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub fn prepare_ocr_png(image_path: &Path) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(image_path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.len() > 64 * 1024 * 1024
    {
        return Err("OCR image unavailable.".to_string());
    }
    let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
    if !decodable_png(&bytes) {
        return Err("Invalid PNG for OCR.".to_string());
    }
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    if w.max(h) <= 2400 {
        return Ok(bytes);
    }
    let (nw, nh) = if w >= h {
        (2400, ((h as u64 * 2400) / w as u64).max(1) as u32)
    } else {
        (((w as u64 * 2400) / h as u64).max(1) as u32, 2400)
    };
    let resized = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn recognize_with_tesseract(
    tesseract_bin: Option<&str>,
    image_path: &Path,
) -> Result<String, String> {
    let Some(bin) = tesseract_bin else {
        return Ok(String::new());
    };
    let png_bytes = prepare_ocr_png(image_path)?;
    let out = read_process(
        bin,
        &["stdin", "stdout", "-l", "eng+tur", "--loglevel", "ERROR"],
        256 * 1024,
        30_000,
        Some(&png_bytes),
    )
    .or_else(|_| {
        read_process(
            bin,
            &["stdin", "stdout", "-l", "eng", "--loglevel", "ERROR"],
            256 * 1024,
            30_000,
            Some(&png_bytes),
        )
    })
    .map_err(|e| e.message)?;

    let text: String = String::from_utf8_lossy(&out)
        .trim()
        .chars()
        .take(10000)
        .collect();
    Ok(text)
}
