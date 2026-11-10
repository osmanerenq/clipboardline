use crate::model::{recording_metadata, MAXIMUM_VIDEO_BYTES};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

pub const MAXIMUM_JOB_MS: u64 = 10 * 60 * 1000;
pub const OWNER: &str = "clipboardline-media-v1";
pub const MARKER: &str = ".clipboardline-media.json";
pub const INPUT_ARGS: [&str; 4] = ["-f", "mov", "-protocol_whitelist", "file"];
pub const VIDEO_ENCODERS: [&str; 3] = ["libx264", "libopenh264", "libvpx-vp9"];
pub const AUDIO_ENCODERS: [&str; 3] = ["aac", "libfdk_aac", "libopus"];

fn encoder_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[VAS][A-Z.]{5}\s+(\S+)").unwrap())
}

fn job_dir_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^job-[A-Za-z0-9]{6}$").unwrap())
}

#[derive(Debug, Clone)]
pub struct MediaError {
    pub message: String,
    pub code: String,
    pub stage: Option<String>,
    pub aborted: bool,
    pub missing: bool,
    pub exit_code: Option<i32>,
}

impl MediaError {
    pub fn failure(msg: impl Into<String>, stage: Option<&str>) -> Self {
        Self {
            message: msg.into(),
            code: "MEDIA_UNSUPPORTED_OR_FAILED".to_string(),
            stage: stage.map(|s| s.to_string()),
            aborted: false,
            missing: false,
            exit_code: None,
        }
    }

    pub fn aborted() -> Self {
        Self {
            message: "Video operation canceled.".to_string(),
            code: "AbortError".to_string(),
            stage: None,
            aborted: true,
            missing: false,
            exit_code: None,
        }
    }

    pub fn expired(stage: &str) -> Self {
        Self::failure("Video operation timed out after ten minutes.", Some(stage))
    }
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoDescription {
    #[serde(rename = "durationMs")]
    pub duration_ms: f64,
    pub width: i64,
    pub height: i64,
    #[serde(rename = "videoCodec")]
    pub video_codec: String,
    #[serde(rename = "audioTracks")]
    pub audio_tracks: i64,
    #[serde(rename = "bitRate")]
    pub bit_rate: i64,
}

fn stream_rotation(stream: &Value) -> i64 {
    if let Some(list) = stream.get("side_data_list").and_then(|v| v.as_array()) {
        for entry in list {
            if let Some(rot) = entry.get("rotation") {
                if let Some(n) = rot
                    .as_i64()
                    .or_else(|| rot.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
                    .or_else(|| rot.as_str().and_then(|s| s.parse::<i64>().ok()))
                {
                    return n;
                }
            }
        }
    }
    stream
        .get("tags")
        .and_then(|t| t.get("rotate"))
        .and_then(|r| {
            r.as_i64()
                .or_else(|| r.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .unwrap_or(0)
}

fn parse_f64(val: Option<&Value>) -> Option<f64> {
    let v = val?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        .filter(|n| n.is_finite())
}

fn parse_i64(val: Option<&Value>) -> Option<i64> {
    let v = val?;
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

pub fn describe(probe: &Value) -> Result<VideoDescription, MediaError> {
    let streams = probe
        .get("streams")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(video) = streams.iter().find(|s| {
        s.get("codec_type").and_then(|v| v.as_str()) == Some("video")
            && s.get("disposition")
                .and_then(|d| d.get("attached_pic"))
                .and_then(|a| a.as_i64())
                != Some(1)
    }) else {
        return Err(MediaError::failure(
            "The recording has no usable video track.",
            Some("Decode"),
        ));
    };

    let seconds = parse_f64(probe.get("format").and_then(|f| f.get("duration")))
        .filter(|&d| d > 0.0)
        .or_else(|| parse_f64(video.get("duration")).filter(|&d| d > 0.0))
        .unwrap_or(0.0);
    let mut width = parse_i64(video.get("width")).unwrap_or(0);
    let mut height = parse_i64(video.get("height")).unwrap_or(0);
    if stream_rotation(video).abs() % 180 == 90 {
        std::mem::swap(&mut width, &mut height);
    }
    let duration_ms = (seconds * 1_000_000.0).round() / 1000.0;
    if !duration_ms.is_finite() || duration_ms <= 0.0 || width < 1 || height < 1 {
        return Err(MediaError::failure(
            "The recording has no usable video track.",
            Some("Decode"),
        ));
    }
    let video_codec: String = video
        .get("codec_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    let audio_tracks = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("audio"))
        .count() as i64;
    let bit_rate = parse_i64(video.get("bit_rate"))
        .filter(|&b| b > 0)
        .or_else(|| parse_i64(probe.get("format").and_then(|f| f.get("bit_rate"))))
        .unwrap_or(0);

    Ok(VideoDescription {
        duration_ms,
        width,
        height,
        video_codec,
        audio_tracks,
        bit_rate,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableEncoders {
    pub videos: Vec<String>,
    pub audios: Vec<String>,
}

pub fn choose_encoders(listing: &str) -> AvailableEncoders {
    let mut names = HashSet::new();
    for line in listing.lines() {
        if let Some(caps) = encoder_line_re().captures(line) {
            if let Some(m) = caps.get(1) {
                names.insert(m.as_str().to_string());
            }
        }
    }
    let videos = VIDEO_ENCODERS
        .iter()
        .filter(|&&n| names.contains(n))
        .map(|&s| s.to_string())
        .collect();
    let audios = AUDIO_ENCODERS
        .iter()
        .filter(|&&n| names.contains(n))
        .map(|&s| s.to_string())
        .collect();
    AvailableEncoders { videos, audios }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderAttempt {
    pub video: String,
    pub audio: Option<String>,
}

pub fn encoder_attempts(available: &AvailableEncoders, audio_tracks: i64) -> Vec<EncoderAttempt> {
    let mut attempts = Vec::new();
    for v in &available.videos {
        if audio_tracks > 0 {
            for a in &available.audios {
                attempts.push(EncoderAttempt {
                    video: v.clone(),
                    audio: Some(a.clone()),
                });
            }
        } else {
            attempts.push(EncoderAttempt {
                video: v.clone(),
                audio: None,
            });
        }
    }
    attempts.truncate(4);
    attempts
}

pub fn trim_arguments(
    source: &str,
    output: &str,
    start_ms: f64,
    end_ms: f64,
    encoders: &EncoderAttempt,
    details: &VideoDescription,
) -> Vec<String> {
    let video_args: Vec<String> = if encoders.video == "libx264" {
        vec![
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            "-crf".into(),
            "20".into(),
        ]
    } else if encoders.video == "libopenh264" {
        let br = if details.bit_rate > 0 {
            details.bit_rate
        } else {
            8_000_000
        }
        .clamp(1_000_000, 40_000_000);
        vec![
            "-c:v".into(),
            "libopenh264".into(),
            "-b:v".into(),
            br.to_string(),
        ]
    } else {
        vec![
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-deadline".into(),
            "realtime".into(),
            "-cpu-used".into(),
            "8".into(),
            "-row-mt".into(),
            "1".into(),
            "-crf".into(),
            "32".into(),
            "-b:v".into(),
            "0".into(),
        ]
    };

    let audio_args: Vec<String> = if details.audio_tracks > 0 && encoders.audio.is_some() {
        let aud = encoders.audio.as_ref().unwrap();
        let ar = if aud == "libopus" { "48000" } else { "44100" };
        vec![
            "-map".into(),
            "0:a:0".into(),
            "-c:a".into(),
            aud.clone(),
            "-b:a".into(),
            "128k".into(),
            "-ar".into(),
            ar.into(),
            "-ac".into(),
            "2".into(),
        ]
    } else {
        vec!["-an".into()]
    };

    let mut out = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-ss".into(),
        format!("{:.3}", start_ms / 1000.0),
    ];
    for a in INPUT_ARGS {
        out.push(a.to_string());
    }
    out.extend([
        "-i".into(),
        source.into(),
        "-t".into(),
        format!("{:.3}", (end_ms - start_ms) / 1000.0),
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        "scale=trunc(iw/2)*2:trunc(ih/2)*2".into(),
    ]);
    out.extend(video_args);
    out.extend(["-pix_fmt".into(), "yuv420p".into()]);
    out.extend(audio_args);
    out.extend(["-movflags".into(), "+faststart".into(), "-y".into(), output.into()]);
    out
}

pub fn run_tool(
    file: &str,
    args: &[String],
    aborted: Option<&Arc<AtomicBool>>,
    timeout_ms: u64,
    limit: usize,
    stage: &str,
) -> Result<String, MediaError> {
    if aborted.map(|a| a.load(Ordering::SeqCst)).unwrap_or(false) {
        return Err(MediaError::aborted());
    }
    if timeout_ms == 0 {
        return Err(MediaError::expired(stage));
    }
    let mut child = Command::new(file)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            let missing = e.kind() == std::io::ErrorKind::NotFound;
            let mut err = MediaError::failure(
                if missing {
                    "Install FFmpeg (ffmpeg and ffprobe) to import or trim recordings on Linux."
                        .to_string()
                } else {
                    format!("FFmpeg could not start: {}", e)
                },
                Some(stage),
            );
            err.missing = missing;
            err
        })?;

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
                    buf.extend_from_slice(&chunk[..n]);
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
        let mut buf = Vec::new();
        let _ = stderr.take(8000).read_to_end(&mut buf);
        let _ = tx_err.send(String::from_utf8_lossy(&buf).to_string());
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if aborted.map(|a| a.load(Ordering::SeqCst)).unwrap_or(false) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MediaError::aborted());
        }
        if let Ok((_, true)) = rx_out.try_recv() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(MediaError::failure("Invalid FFmpeg response.", Some(stage)));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let (out_bytes, overflow) = rx_out
                    .recv_timeout(Duration::from_millis(200))
                    .unwrap_or((Vec::new(), false));
                if overflow {
                    return Err(MediaError::failure("Invalid FFmpeg response.", Some(stage)));
                }
                if status.success() {
                    return Ok(String::from_utf8_lossy(&out_bytes).to_string());
                }
                let stderr_str = rx_err
                    .recv_timeout(Duration::from_millis(200))
                    .unwrap_or_default();
                let code = status.code().unwrap_or(1);
                let last_line = stderr_str
                    .trim()
                    .lines()
                    .last()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("exit {}", code));
                let msg: String = format!("FFmpeg could not process this recording: {}", last_line)
                    .chars()
                    .take(500)
                    .collect();
                let mut err = MediaError::failure(msg, Some(stage));
                err.exit_code = Some(code);
                return Err(err);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(MediaError::expired(stage));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(MediaError::failure(
                    format!("FFmpeg could not start: {}", e),
                    Some(stage),
                ));
            }
        }
    }
}

pub struct LinuxMedia {
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
    encoders: Mutex<Option<AvailableEncoders>>,
}

impl LinuxMedia {
    pub fn new(ffmpeg: Option<String>, ffprobe: Option<String>) -> Self {
        Self {
            ffmpeg,
            ffprobe,
            encoders: Mutex::new(None),
        }
    }

    fn tool(
        &self,
        is_ffprobe: bool,
        args: &[String],
        aborted: Option<&Arc<AtomicBool>>,
        timeout_ms: u64,
        stage: &str,
    ) -> Result<String, MediaError> {
        let bin = if is_ffprobe {
            self.ffprobe.as_deref()
        } else {
            self.ffmpeg.as_deref()
        };
        let Some(file) = bin else {
            let mut err = MediaError::failure(
                "Install FFmpeg (ffmpeg and ffprobe) to import or trim recordings on Linux.",
                Some("Runtime"),
            );
            err.missing = true;
            return Err(err);
        };
        run_tool(file, args, aborted, timeout_ms, 4 * 1024 * 1024, stage)
    }

    pub fn probe(
        &self,
        file: &str,
        aborted: Option<&Arc<AtomicBool>>,
        timeout_ms: u64,
    ) -> Result<VideoDescription, MediaError> {
        let mut args = vec!["-v".to_string(), "error".to_string()];
        for a in INPUT_ARGS {
            args.push(a.to_string());
        }
        args.extend([
            "-print_format".to_string(),
            "json".to_string(),
            "-show_format".to_string(),
            "-show_streams".to_string(),
            file.to_string(),
        ]);
        let output = self.tool(true, &args, aborted, timeout_ms, "Decode")?;
        let parsed: Value = serde_json::from_str(&output)
            .map_err(|_| MediaError::failure("Invalid FFmpeg response.", Some("Decode")))?;
        describe(&parsed)
    }

    pub fn run(
        &self,
        mode: &str,
        source: &str,
        output_directory: &Path,
        start_ms: Option<f64>,
        end_ms: Option<f64>,
        verify_audio: bool,
        aborted: Option<&Arc<AtomicBool>>,
        timeout_ms: u64,
    ) -> Result<Value, MediaError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let remaining = || -> u64 {
            deadline
                .checked_duration_since(Instant::now())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };

        let mut details = self.probe(source, aborted, remaining())?;
        let mut poster_source = source.to_string();
        let mut trimmed_file: Option<String> = None;

        if mode == "Trim" {
            let s_ms = start_ms.unwrap_or(-1.0);
            let e_ms = end_ms.unwrap_or(-1.0);
            if !s_ms.is_finite()
                || !e_ms.is_finite()
                || s_ms < 0.0
                || e_ms > details.duration_ms + 1.0
                || e_ms - s_ms < 50.0
            {
                return Err(MediaError::failure(
                    "Choose a nonempty interval within this recording.",
                    Some("Trim"),
                ));
            }
            let available = {
                let mut guard = self.encoders.lock().unwrap();
                if guard.is_none() {
                    let listing = self.tool(
                        false,
                        &["-hide_banner".to_string(), "-encoders".to_string()],
                        aborted,
                        remaining(),
                        "Trim",
                    )?;
                    *guard = Some(choose_encoders(&listing));
                }
                guard.clone().unwrap()
            };
            let attempts = encoder_attempts(&available, details.audio_tracks);
            if attempts.is_empty() {
                let msg = if !available.videos.is_empty() {
                    "This FFmpeg build has no AAC or Opus audio encoder."
                } else {
                    "This FFmpeg build has no H.264 or VP9 encoder."
                };
                return Err(MediaError::failure(msg, Some("Trim")));
            }
            let output = output_directory.join("trimmed.mp4");
            let output_str = output.to_string_lossy().to_string();
            let mut last_problem = None;
            for enc in &attempts {
                let args = trim_arguments(source, &output_str, s_ms, e_ms, enc, &details);
                match self.tool(false, &args, aborted, remaining(), "TrimRender") {
                    Ok(_) => {
                        last_problem = None;
                        break;
                    }
                    Err(e) => {
                        if e.aborted || aborted.map(|a| a.load(Ordering::SeqCst)).unwrap_or(false)
                        {
                            return Err(e);
                        }
                        last_problem = Some(e);
                    }
                }
            }
            if let Some(err) = last_problem {
                return Err(err);
            }
            details = self.probe(&output_str, aborted, remaining())?;
            poster_source = output_str;
            if (details.duration_ms - (e_ms - s_ms)).abs() > 150.0 {
                return Err(MediaError::failure(
                    "The encoded trim duration did not match the selection.",
                    Some("TrimReopen"),
                ));
            }
            trimmed_file = Some("trimmed.mp4".to_string());
        }

        let poster_path = output_directory.join("poster.png");
        let mut poster_args = vec![
            "-nostdin".to_string(),
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
        ];
        for a in INPUT_ARGS {
            poster_args.push(a.to_string());
        }
        poster_args.extend([
            "-i".to_string(),
            poster_source.clone(),
            "-frames:v".to_string(),
            "1".to_string(),
            "-vf".to_string(),
            "scale=320:-2".to_string(),
            "-c:v".to_string(),
            "png".to_string(),
            "-f".to_string(),
            "image2".to_string(),
            "-y".to_string(),
            poster_path.to_string_lossy().to_string(),
        ]);
        self.tool(false, &poster_args, aborted, remaining(), "Poster")?;

        let mut audio_file: Option<String> = None;
        if verify_audio && details.audio_tracks > 0 {
            let audio_path = output_directory.join("audio.wav");
            let mut audio_args = vec![
                "-nostdin".to_string(),
                "-hide_banner".to_string(),
                "-loglevel".to_string(),
                "error".to_string(),
            ];
            for a in INPUT_ARGS {
                audio_args.push(a.to_string());
            }
            audio_args.extend([
                "-i".to_string(),
                poster_source,
                "-vn".to_string(),
                "-map".to_string(),
                "0:a:0".to_string(),
                "-ac".to_string(),
                "1".to_string(),
                "-ar".to_string(),
                "44100".to_string(),
                "-c:a".to_string(),
                "pcm_s16le".to_string(),
                "-y".to_string(),
                audio_path.to_string_lossy().to_string(),
            ]);
            self.tool(false, &audio_args, aborted, remaining(), "AudioVerify")?;
            audio_file = Some("audio.wav".to_string());
        }

        if !poster_path.exists() {
            return Err(MediaError::failure(
                "Video poster is unavailable.",
                Some("Poster"),
            ));
        }

        let mut out = json!({
            "durationMs": details.duration_ms,
            "width": details.width,
            "height": details.height,
            "videoCodec": details.video_codec,
            "audioTracks": details.audio_tracks,
            "posterFile": "poster.png",
            "ok": true,
            "engine": "FFmpeg"
        });
        if let Some(tf) = trimmed_file {
            out.as_object_mut()
                .unwrap()
                .insert("trimmedFile".to_string(), json!(tf));
        }
        if let Some(af) = audio_file {
            out.as_object_mut()
                .unwrap()
                .insert("audioFile".to_string(), json!(af));
        }
        Ok(out)
    }
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn ensure_safe_directory(dir: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|_| "Unsafe private video directory.".to_string())?;
    if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
        return Err("Unsafe private video directory.".to_string());
    }
    Ok(())
}

pub fn valid_video(file: &Path) -> Result<fs::Metadata, String> {
    if !file.is_absolute() {
        return Err("Recording is unavailable.".to_string());
    }
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    if ext != ".mov" && ext != ".mp4" || !is_regular_file(file) {
        return Err("Recording is unavailable.".to_string());
    }
    let stat = fs::metadata(file).map_err(|_| "Recording is unavailable.".to_string())?;
    if stat.len() < 12 || stat.len() > MAXIMUM_VIDEO_BYTES {
        return Err("Recording exceeds the 2 GB import limit.".to_string());
    }
    Ok(stat)
}

pub struct MediaJobs {
    pub directory: PathBuf,
    pub timeout_ms: u64,
    pub linux_media: LinuxMedia,
}

impl MediaJobs {
    pub fn new(
        directory: impl AsRef<Path>,
        ffmpeg: Option<String>,
        ffprobe: Option<String>,
        timeout_ms: u64,
    ) -> Result<Self, String> {
        let dir = directory.as_ref().to_path_buf();
        if !dir.is_absolute() {
            return Err("Invalid private video directory.".to_string());
        }
        if let Some(parent) = dir.parent() {
            ensure_safe_directory(parent)?;
        }
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        ensure_safe_directory(&dir)?;

        let jobs = Self {
            directory: dir.clone(),
            timeout_ms: timeout_ms.clamp(1, MAXIMUM_JOB_MS),
            linux_media: LinuxMedia::new(ffmpeg, ffprobe),
        };
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if job_dir_re().is_match(name) {
                        jobs.remove(&dir.join(name));
                    }
                }
            }
        }
        Ok(jobs)
    }

    pub fn remove(&self, job_dir: &Path) -> bool {
        let Some(parent) = job_dir.parent() else {
            return false;
        };
        let Some(base) = job_dir.file_name().and_then(|s| s.to_str()) else {
            return false;
        };
        if parent != self.directory || !job_dir_re().is_match(base) {
            return false;
        }
        if ensure_safe_directory(job_dir).is_err() {
            return false;
        }
        let marker = job_dir.join(MARKER);
        if !is_regular_file(&marker) {
            return false;
        }
        let Ok(meta) = fs::metadata(&marker) else {
            return false;
        };
        if meta.len() > 1000 {
            return false;
        }
        let Ok(raw) = fs::read_to_string(&marker) else {
            return false;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        if parsed.get("owner").and_then(|v| v.as_str()) != Some(OWNER) {
            return false;
        }
        let Ok(entries) = fs::read_dir(job_dir) else {
            return false;
        };
        let mut names = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                return false;
            };
            if ![MARKER, "poster.png", "trimmed.mp4", "audio.wav"].contains(&name.as_str())
                || !is_regular_file(&job_dir.join(&name))
            {
                return false;
            }
            names.push(name);
        }
        for name in names {
            if fs::remove_file(job_dir.join(name)).is_err() {
                return false;
            }
        }
        fs::remove_dir(job_dir).is_ok()
    }

    fn create_job_dir(&self) -> Result<PathBuf, String> {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        for _ in 0..64 {
            let mut suffix = String::with_capacity(6);
            for _ in 0..6 {
                let idx = (rand::random::<u8>() as usize) % CHARS.len();
                suffix.push(CHARS[idx] as char);
            }
            let candidate = self.directory.join(format!("job-{}", suffix));
            if fs::create_dir(&candidate).is_ok() {
                let _ = fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700));
                let marker_path = candidate.join(MARKER);
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&marker_path)
                    .map_err(|e| e.to_string())?;
                f.write_all(json!({ "owner": OWNER }).to_string().as_bytes())
                    .map_err(|e| e.to_string())?;
                return Ok(candidate);
            }
        }
        Err("Could not allocate temporary video directory.".to_string())
    }

    pub fn execute(
        &self,
        mode: &str,
        source: &Path,
        start_ms: Option<f64>,
        end_ms: Option<f64>,
        verify_audio: bool,
        aborted: Option<&Arc<AtomicBool>>,
    ) -> Result<Value, String> {
        if aborted.map(|a| a.load(Ordering::SeqCst)).unwrap_or(false) {
            return Err("Video operation canceled.".to_string());
        }
        let before = valid_video(source)?;
        ensure_safe_directory(&self.directory)?;
        if mode == "Trim" {
            let s = start_ms.unwrap_or(-1.0);
            let e = end_ms.unwrap_or(-1.0);
            if !s.is_finite() || !e.is_finite() || s < 0.0 || e - s < 50.0 {
                return Err("Choose a nonempty trim interval.".to_string());
            }
        }
        let job_dir = self.create_job_dir()?;
        let mut keep = false;
        let res = (|| -> Result<Value, String> {
            let source_str = source.to_string_lossy().to_string();
            let result = self
                .linux_media
                .run(
                    mode,
                    &source_str,
                    &job_dir,
                    start_ms,
                    end_ms,
                    verify_audio,
                    aborted,
                    self.timeout_ms,
                )
                .map_err(|e| e.message)?;

            if aborted.map(|a| a.load(Ordering::SeqCst)).unwrap_or(false) {
                return Err("Video operation canceled.".to_string());
            }
            let after = valid_video(source)?;
            if before.len() != after.len()
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
                || before.ino() != after.ino()
            {
                return Err("The recording changed while processing.".to_string());
            }
            let details = recording_metadata(&result).map_err(|e| e.message)?;
            let poster = job_dir.join("poster.png");
            if result.get("posterFile").and_then(|v| v.as_str()) != Some("poster.png")
                || !is_regular_file(&poster)
                || fs::metadata(&poster).map(|m| m.len()).unwrap_or(u64::MAX) > 4 * 1024 * 1024
            {
                return Err("Video poster is unavailable.".to_string());
            }
            let poster_png =
                fs::read(&poster).map_err(|_| "Video poster is unavailable.".to_string())?;
            if poster_png.len() < 8 || &poster_png[0..8] != b"\x89PNG\r\n\x1a\n" {
                return Err("Invalid video poster.".to_string());
            }

            let mut out = Value::Object(details);
            let obj = out.as_object_mut().unwrap();
            obj.insert(
                "posterFile".to_string(),
                json!(poster.to_string_lossy().to_string()),
            );
            obj.insert(
                "jobDirectory".to_string(),
                json!(job_dir.to_string_lossy().to_string()),
            );

            if let Some(af) = result.get("audioFile").and_then(|v| v.as_str()) {
                let audio_path = job_dir.join("audio.wav");
                if af != "audio.wav"
                    || !is_regular_file(&audio_path)
                    || fs::metadata(&audio_path)
                        .map(|m| m.len())
                        .unwrap_or(u64::MAX)
                        > 32 * 1024 * 1024
                {
                    return Err("Invalid native audio verification.".to_string());
                }
                obj.insert(
                    "audioFile".to_string(),
                    json!(audio_path.to_string_lossy().to_string()),
                );
            }

            if mode == "Trim" {
                let trimmed = job_dir.join("trimmed.mp4");
                if result.get("trimmedFile").and_then(|v| v.as_str()) != Some("trimmed.mp4") {
                    return Err("Invalid recording output.".to_string());
                }
                valid_video(&trimmed)?;
                obj.insert(
                    "file".to_string(),
                    json!(trimmed.to_string_lossy().to_string()),
                );
            } else {
                obj.insert("file".to_string(), json!(source_str));
            }
            keep = true;
            Ok(out)
        })();

        if !keep {
            self.remove(&job_dir);
        }
        res
    }

    pub fn release(&self, result: &Value) -> bool {
        let Some(jd) = result.get("jobDirectory").and_then(|v| v.as_str()) else {
            return false;
        };
        self.remove(Path::new(jd))
    }
}
