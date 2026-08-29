//! Image / audio / video parts for OpenAI-compat + official Qwen3.8 Jinja.
//!
//! QwenPaw `view_image` / `view_video` return a media block plus a short text
//! hint. Local files freeze to a data URI (2 MB cap). HTTP(S) URLs pass through.
//! Official Jinja matches `image_url` and `'video' in item`; `video_url` alone
//! does not. Audio is not in the 3.8 template — local metering uses text.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// QwenPaw `MAX_INLINE_MEDIA_BYTES`.
pub const MAX_INLINE_MEDIA_BYTES: usize = 2 * 1024 * 1024;

/// 32×32 solid red PNG (96 bytes). Same payload as QwenPaw `multimodal_prober`.
pub const PROBE_IMAGE_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJ0lEQVR42u3NsQkAAAjAsP7/tF7hIASyp6lTCQQCgUAgEAgEgi/BAjLD/C5w/SM9AAAAAElFTkSuQmCC";

pub const PROBE_VIDEO_URL: &str =
    "https://help-static-aliyun-doc.aliyuncs.com/file-manage-files/zh-CN/20241115/cqqkru/1.mp4";

pub const IMAGE_PROBE_PROMPT: &str =
    "What is the single dominant color of this image? Reply with ONLY the color name, nothing else.";

pub const VIDEO_PROBE_PROMPT: &str =
    "What is the single dominant color in this video? Reply with ONLY the color name, nothing else.";

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];
const VIDEO_EXT: &[&str] = &["mp4", "webm", "mpeg", "mpg", "mov", "avi", "mkv"];
const AUDIO_EXT: &[&str] = &["wav", "mp3", "flac", "ogg", "m4a", "aac", "opus", "wma"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    #[default]
    Image,
    Audio,
    Video,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "image" | "img" | "picture" | "photo" => Some(Self::Image),
            "audio" | "sound" | "voice" => Some(Self::Audio),
            "video" | "movie" => Some(Self::Video),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPart {
    pub kind: MediaKind,
    pub mime: String,
    pub url: String,
}

impl MediaPart {
    pub fn image_url(url: impl Into<String>) -> Self {
        let url = url.into();
        let mime = mime_from_url(&url).unwrap_or("image/png");
        Self {
            kind: MediaKind::Image,
            mime: mime.into(),
            url,
        }
    }

    pub fn video_url(url: impl Into<String>) -> Self {
        let url = url.into();
        let mime = mime_from_url(&url).unwrap_or("video/mp4");
        Self {
            kind: MediaKind::Video,
            mime: mime.into(),
            url,
        }
    }

    pub fn audio_url(url: impl Into<String>, mime: impl Into<String>) -> Self {
        Self {
            kind: MediaKind::Audio,
            mime: mime.into(),
            url: url.into(),
        }
    }

    pub fn data_uri(kind: MediaKind, mime: &str, bytes: &[u8]) -> Self {
        Self {
            kind,
            mime: mime.to_string(),
            url: data_uri(mime, bytes),
        }
    }

    /// OpenAI-compat body. Extra `video` key so official Jinja `'video' in item` matches.
    pub fn to_api_value(&self) -> Value {
        match self.kind {
            MediaKind::Image => json!({
                "type": "image_url",
                "image_url": { "url": self.url },
            }),
            MediaKind::Video => json!({
                "type": "video",
                "video": self.url,
            }),
            MediaKind::Audio => audio_api_value(&self.url, &self.mime),
        }
    }

    /// Official 3.8 Jinja: images via `image_url`, videos via `type=video`.
    /// Audio is not in the template — emit a text placeholder.
    pub fn to_jinja_value(&self) -> Value {
        match self.kind {
            MediaKind::Image => json!({
                "type": "image_url",
                "image_url": { "url": self.url },
            }),
            MediaKind::Video => json!({
                "type": "video",
                "video": self.url,
            }),
            MediaKind::Audio => json!({
                "type": "text",
                "text": "[audio]",
            }),
        }
    }
}

/// Probe-filled attach flags. `None` = unknown.
/// Images attach when unknown (this family ships a vision encoder; probe.json
/// from before MM fields would otherwise disable `view`). Video/audio stay off
/// until probe says yes — llama.cpp on this box rejects both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaCaps {
    pub image: Option<bool>,
    pub video: Option<bool>,
    pub audio: Option<bool>,
    pub transcription: Option<bool>,
    /// OpenAI-compat origin (`…/v1`) and bearer key for transcriptions.
    pub origin: Option<(String, String)>,
}

impl MediaCaps {
    pub fn attach_image(&self) -> bool {
        self.image != Some(false)
    }

    pub fn attach_video(&self) -> bool {
        self.video == Some(true)
    }

    pub fn attach_audio(&self) -> bool {
        self.audio == Some(true)
    }

    pub fn try_transcribe(&self) -> bool {
        self.transcription == Some(true)
    }

    pub fn any(&self) -> bool {
        self.attach_image() || self.attach_video() || self.attach_audio()
    }
}

/// Local helpers for video stills (`ffmpeg`) and audio transcripts (`whisper-cli`).
///
/// Lookup is PATH + a few well-known install dirs on Windows / Linux / macOS.
/// Never assumes Homebrew. Override with config or `HYPER_FFMPEG` / `HYPER_WHISPER` /
/// `HYPER_WHISPER_MODEL`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaBins {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub whisper: Option<PathBuf>,
    pub whisper_model: Option<PathBuf>,
}

impl MediaBins {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn detect() -> Self {
        Self::resolve("", "", "")
    }

    pub fn from_config(c: &crate::config::MediaConfig) -> Self {
        Self::resolve(&c.ffmpeg, &c.whisper, &c.whisper_model)
    }

    /// Config paths first, then env (`HYPER_*` wins), then PATH / extra dirs.
    pub fn resolve(ffmpeg: &str, whisper: &str, model: &str) -> Self {
        let ffmpeg_explicit = first_nonempty(&[env_nonempty("HYPER_FFMPEG"), nonempty(ffmpeg)]);
        let whisper_explicit = first_nonempty(&[env_nonempty("HYPER_WHISPER"), nonempty(whisper)]);
        let model_explicit =
            first_nonempty(&[env_nonempty("HYPER_WHISPER_MODEL"), nonempty(model)]);
        Self {
            ffmpeg: resolve_bin(ffmpeg_explicit.as_deref(), &["ffmpeg"]),
            ffprobe: sibling_or_find(ffmpeg_explicit.as_deref(), "ffprobe"),
            whisper: resolve_bin(whisper_explicit.as_deref(), &["whisper-cli", "whisper-cpp"]),
            whisper_model: resolve_model(model_explicit.as_deref()),
        }
    }

    pub fn expected_whisper_model() -> PathBuf {
        hyper_home()
            .unwrap_or_else(|| PathBuf::from(".grok-hyper"))
            .join("whisper")
            .join("ggml-tiny.bin")
    }
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|v| nonempty(&v))
}

fn first_nonempty(items: &[Option<String>]) -> Option<String> {
    items.iter().cloned().find_map(|x| x)
}

fn hyper_home() -> Option<PathBuf> {
    crate::config::user_home().map(|h| h.join(".grok-hyper"))
}

fn resolve_bin(explicit: Option<&str>, names: &[&str]) -> Option<PathBuf> {
    if let Some(raw) = explicit {
        let p = PathBuf::from(raw);
        if p.is_dir() {
            for name in names {
                if let Some(hit) = bin_in_dir(&p, name) {
                    return Some(hit);
                }
            }
            return None;
        }
        if p.is_file() {
            return Some(p);
        }
        // Bare command name in config/env.
        if !raw.contains('/') && !raw.contains('\\') {
            if let Some(hit) = find_bin(raw) {
                return Some(hit);
            }
        }
        return Some(p);
    }
    names.iter().find_map(|n| find_bin(n))
}

fn sibling_or_find(explicit_ffmpeg: Option<&str>, name: &str) -> Option<PathBuf> {
    if let Some(raw) = explicit_ffmpeg {
        let p = PathBuf::from(raw);
        if p.is_dir() {
            if let Some(hit) = bin_in_dir(&p, name) {
                return Some(hit);
            }
        } else if let Some(parent) = p.parent() {
            if let Some(hit) = bin_in_dir(parent, name) {
                return Some(hit);
            }
        }
    }
    find_bin(name)
}

fn resolve_model(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(raw) = explicit {
        let p = PathBuf::from(raw);
        if p.is_dir() {
            return whisper_model_in_dir(&p);
        }
        if p.is_file() {
            return Some(p);
        }
        return Some(p);
    }
    let dir = hyper_home()?.join("whisper");
    whisper_model_in_dir(&dir)
}

fn whisper_model_in_dir(dir: &Path) -> Option<PathBuf> {
    for name in [
        "ggml-tiny.bin",
        "ggml-tiny.en.bin",
        "ggml-base.bin",
        "ggml-base.en.bin",
    ] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// PATH, then OS-typical install dirs. Checks `PATHEXT` on Windows.
pub fn find_bin(name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(extra_bin_dirs());
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        if !seen.insert(dir.clone()) {
            continue;
        }
        if let Some(hit) = bin_in_dir(&dir, name) {
            return Some(hit);
        }
    }
    None
}

fn bin_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    for cand in bin_candidates(dir, name) {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn bin_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut out = vec![dir.join(name)];
    let already_ext = Path::new(name).extension().is_some();
    if already_ext {
        return out;
    }
    #[cfg(windows)]
    {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into());
        for raw in pathext.split(';') {
            let ext = raw.trim();
            if ext.is_empty() {
                continue;
            }
            let ext = if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            };
            out.push(dir.join(format!("{name}{ext}")));
        }
    }
    out
}

fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(unix)]
    {
        dirs.extend([
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/opt/local/bin"),
            PathBuf::from("/snap/bin"),
        ]);
        if let Some(h) = crate::config::user_home() {
            dirs.push(h.join(".local").join("bin"));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(pd) = std::env::var("ProgramData") {
            dirs.push(PathBuf::from(pd).join("chocolatey").join("bin"));
        }
        if let Ok(la) = std::env::var("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(la)
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links"),
            );
        }
        if let Some(h) = crate::config::user_home() {
            dirs.push(h.join("scoop").join("shims"));
            dirs.push(h.join(".local").join("bin"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            let pf = PathBuf::from(pf);
            dirs.push(pf.join("ffmpeg").join("bin"));
            dirs.push(pf.join("whisper.cpp").join("bin"));
            dirs.push(pf.join("whisper-cpp").join("bin"));
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            dirs.push(PathBuf::from(pf86).join("ffmpeg").join("bin"));
        }
        dirs.push(PathBuf::from(r"C:\ffmpeg\bin"));
    }
    dirs
}

pub fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", B64.encode(bytes))
}

pub fn decode_data_uri(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(";base64,")?;
    let mime = meta.split(';').next().unwrap_or(meta).to_string();
    let bytes = B64.decode(b64.as_bytes()).ok()?;
    Some((mime, bytes))
}

pub fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    sniff_known_image_mime(bytes).unwrap_or("image/jpeg")
}

fn sniff_known_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn ext_for_image_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

/// Host `image_generation_call.result` is raw base64 with no data-URL prefix.
pub fn decode_image_payload(raw: &str) -> Option<(String, Vec<u8>)> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(hit) = decode_data_uri(t) {
        return Some(hit);
    }
    let cleaned: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = B64.decode(cleaned.as_bytes()).ok()?;
    if bytes.len() < 24 {
        return None;
    }
    Some((sniff_image_mime(&bytes).to_string(), bytes))
}

pub fn persist_image_file(root: &Path, bytes: &[u8], mime: &str) -> Option<String> {
    let ext = ext_for_image_mime(mime);
    let uniq = format!(
        "imagine-{}.{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8],
        ext
    );
    let rel = format!(".grok-hyper/generated/{uniq}").replace('\\', "/");
    let dest = root.join(".grok-hyper").join("generated").join(&uniq);
    std::fs::create_dir_all(dest.parent()?).ok()?;
    std::fs::write(&dest, bytes).ok()?;
    Some(rel)
}

/// Drop historical images unless the current user pointed at a picture.
/// JSONL / UI media are unchanged; this is wire-only.
///
/// Current-turn attachments and in-turn `view` / imagine shots stay so the
/// model can still see what it just opened. Prior-turn generated JPEGs do not
/// ride along on "write a ppt skill".
pub fn retain_referenced_media(messages: &mut [crate::template::ChatMessage]) {
    let Some(user_i) = latest_real_user(messages) else {
        strip_images(messages);
        return;
    };
    let mentioned = mentions_image(messages[user_i].content.as_deref().unwrap_or(""));
    if !mentioned {
        for (i, msg) in messages.iter_mut().enumerate() {
            if i < user_i {
                msg.parts.retain(|p| p.kind != MediaKind::Image);
            }
        }
    }
    cap_image_parts(messages, 4);
}

fn latest_real_user(messages: &[crate::template::ChatMessage]) -> Option<usize> {
    messages.iter().enumerate().rev().find_map(|(i, m)| {
        if m.role != "user" {
            return None;
        }
        let t = m.content.as_deref().unwrap_or("");
        if crate::template::is_hidden_user_text(t) {
            None
        } else {
            Some(i)
        }
    })
}

fn strip_images(messages: &mut [crate::template::ChatMessage]) {
    for m in messages {
        m.parts.retain(|p| p.kind != MediaKind::Image);
    }
}

fn cap_image_parts(messages: &mut [crate::template::ChatMessage], max: usize) {
    let mut keep = 0usize;
    for m in messages.iter_mut().rev() {
        let parts: Vec<MediaPart> = m.parts.drain(..).collect();
        let image_idx: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == MediaKind::Image)
            .map(|(i, _)| i)
            .collect();
        let mut keep_idx = std::collections::HashSet::new();
        for i in image_idx.into_iter().rev() {
            if keep < max {
                keep_idx.insert(i);
                keep += 1;
            }
        }
        m.parts = parts
            .into_iter()
            .enumerate()
            .filter(|(i, p)| p.kind != MediaKind::Image || keep_idx.contains(i))
            .map(|(_, p)| p)
            .collect();
    }
}

fn has_ascii_word(hay: &str, word: &str) -> bool {
    let h = hay.as_bytes();
    let w = word.as_bytes();
    if w.is_empty() || h.len() < w.len() {
        return false;
    }
    for i in 0..=h.len() - w.len() {
        if &h[i..i + w.len()] != w {
            continue;
        }
        let before = i == 0 || !h[i - 1].is_ascii_alphanumeric();
        let after_i = i + w.len();
        let after = after_i >= h.len() || !h[after_i].is_ascii_alphanumeric();
        if before && after {
            return true;
        }
    }
    false
}

pub fn mentions_image(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    const WORDS: &[&str] = &[
        "image",
        "images",
        "picture",
        "pictures",
        "photo",
        "photos",
        "screenshot",
        "screenshots",
        "diagram",
        "diagrams",
    ];
    if WORDS.iter().any(|w| has_ascii_word(&t, w)) {
        return true;
    }
    const EXT: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"];
    if EXT.iter().any(|k| t.contains(k)) {
        return true;
    }
    const PHRASE: &[&str] = &["this logo", "that logo", "the logo"];
    if PHRASE.iter().any(|k| t.contains(k)) {
        return true;
    }
    const ZH: &[&str] = &[
        "图片",
        "照片",
        "截图",
        "配图",
        "插图",
        "这张图",
        "那张图",
        "看图",
        "见图",
        "刚才的图",
        "上面的图",
        "生成的图",
        "logo图",
    ];
    ZH.iter().any(|k| text.contains(k))
}

/// Turn workspace-relative media paths into data URIs so the next model hop
/// can still see generated / uploaded images.
pub fn inline_workspace_media(
    root: &Path,
    messages: &mut [crate::template::ChatMessage],
    cap: usize,
) {
    for msg in messages {
        for part in &mut msg.parts {
            if part.url.starts_with("data:")
                || part.url.starts_with("http://")
                || part.url.starts_with("https://")
            {
                continue;
            }
            let path = if Path::new(&part.url).is_absolute() {
                PathBuf::from(&part.url)
            } else {
                root.join(&part.url)
            };
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.is_empty() || bytes.len() > cap {
                continue;
            }
            let mime = if part.mime.starts_with("image/") {
                part.mime.clone()
            } else {
                sniff_image_mime(&bytes).to_string()
            };
            part.mime = mime.clone();
            part.url = data_uri(&mime, &bytes);
        }
    }
}

pub async fn fetch_http_bytes(url: &str, cap: usize) -> Result<(String, Vec<u8>), String> {
    let u = url.trim();
    let parsed = reqwest::Url::parse(u).map_err(|e| e.to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("only http(s) urls".into());
    }
    let origin = parsed.origin().ascii_serialization();
    let client = crate::llm_http::apply_env_proxy(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(4))
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36"),
        u,
    )
    .build()
    .map_err(|e| e.to_string())?;
    let resp = client
        .get(parsed)
        .header(
            reqwest::header::ACCEPT,
            "image/avif,image/webp,image/apng,image/*,*/*;q=0.8",
        )
        .header(reqwest::header::REFERER, format!("{origin}/"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status().as_u16()));
    }
    let header_mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > cap {
        return Err("file too large".into());
    }
    if bytes.len() < 24 {
        return Err("not an image".into());
    }
    let sniffed = sniff_known_image_mime(&bytes);
    let mime = if let Some(m) = sniffed {
        m.to_string()
    } else if header_mime.starts_with("image/") && header_mime != "image/svg+xml" {
        header_mime
    } else {
        return Err("not an image".into());
    };
    Ok((mime, bytes.to_vec()))
}

fn audio_api_value(url: &str, mime: &str) -> Value {
    if let Some((_, bytes)) = decode_data_uri(url) {
        let fmt = audio_format(mime, url);
        json!({
            "type": "input_audio",
            "input_audio": {
                "data": B64.encode(bytes),
                "format": fmt,
            }
        })
    } else {
        json!({
            "type": "input_audio",
            "input_audio": { "data": url, "format": audio_format(mime, url) }
        })
    }
}

fn audio_format(mime: &str, url: &str) -> &'static str {
    let blob = format!("{mime} {url}").to_ascii_lowercase();
    if blob.contains("wav") {
        "wav"
    } else if blob.contains("mp3") || blob.contains("mpeg") {
        "mp3"
    } else if blob.contains("flac") {
        "flac"
    } else {
        "wav"
    }
}

pub fn is_http_url(path: &str) -> bool {
    let p = path.trim();
    p.starts_with("http://") || p.starts_with("https://")
}

pub fn path_ext(path: &str) -> String {
    // Only URLs have query strings here. Windows canonical paths start with
    // `\\?\`; splitting every path at `?` erased their extension.
    let cut = if path.starts_with("http://") || path.starts_with("https://") {
        path.split('?').next().unwrap_or(path)
    } else {
        path
    };
    std::path::Path::new(cut)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn kind_from_ext(ext: &str) -> Option<MediaKind> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    if IMAGE_EXT.contains(&e.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXT.contains(&e.as_str()) {
        Some(MediaKind::Video)
    } else if AUDIO_EXT.contains(&e.as_str()) {
        Some(MediaKind::Audio)
    } else {
        None
    }
}

pub fn is_media_ext(path: &str) -> bool {
    kind_from_ext(&path_ext(path)).is_some()
}

pub fn kind_from_magic(bytes: &[u8]) -> Option<MediaKind> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some(MediaKind::Image);
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some(MediaKind::Image);
    }
    if bytes.starts_with(b"GIF8") {
        return Some(MediaKind::Image);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(MediaKind::Image);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        return Some(MediaKind::Audio);
    }
    if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0)
    {
        return Some(MediaKind::Audio);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(MediaKind::Video);
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(MediaKind::Video);
    }
    None
}

pub fn mime_for(kind: MediaKind, ext: &str, bytes: Option<&[u8]>) -> &'static str {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    match kind {
        MediaKind::Image => match e.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "tif" | "tiff" => "image/tiff",
            _ => {
                if let Some(b) = bytes {
                    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
                        return "image/jpeg";
                    }
                    if b.starts_with(b"GIF8") {
                        return "image/gif";
                    }
                    if b.len() >= 12 && &b[8..12] == b"WEBP" {
                        return "image/webp";
                    }
                }
                "image/png"
            }
        },
        MediaKind::Video => match e.as_str() {
            "webm" => "video/webm",
            "mov" => "video/quicktime",
            "avi" => "video/x-msvideo",
            "mkv" => "video/x-matroska",
            _ => "video/mp4",
        },
        MediaKind::Audio => match e.as_str() {
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "ogg" => "audio/ogg",
            "m4a" | "aac" => "audio/mp4",
            "opus" => "audio/opus",
            _ => "audio/wav",
        },
    }
}

fn mime_from_url(url: &str) -> Option<&'static str> {
    let ext = path_ext(url);
    kind_from_ext(&ext).map(|k| mime_for(k, &ext, None))
}

/// Native freeze formats (QwenPaw skips BMP/TIFF conversion without a decoder).
pub fn native_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

pub fn fallback_hint(kind: MediaKind, path: &str, reason: &str) -> String {
    format!(
        "[Note: this endpoint cannot perceive {kind} ({reason}). File: {path}.]",
        kind = kind.as_str(),
    )
}

const RED_KW: &[&str] = &["red", "scarlet", "crimson", "vermilion", "maroon", "红"];
const BLUE_KW: &[&str] = &["blue", "navy", "azure", "cobalt", "cyan", "indigo", "蓝"];

pub fn image_probe_hit(answer: &str, reasoning: &str) -> bool {
    let a = answer.to_ascii_lowercase();
    let r = reasoning.to_ascii_lowercase();
    RED_KW.iter().any(|k| a.contains(k) || r.contains(k))
}

pub fn video_probe_hit(answer: &str, reasoning: &str, http_relaxed: bool) -> bool {
    let a = answer.to_ascii_lowercase();
    let r = reasoning.to_ascii_lowercase();
    if BLUE_KW.iter().any(|k| a.contains(k) || r.contains(k)) {
        return true;
    }
    http_relaxed && !answer.trim().is_empty()
}

/// 20 ms of silence, 8 kHz 16-bit PCM WAV. Used only for native-audio probes.
pub fn silence_wav() -> Vec<u8> {
    let data_len: u32 = 160 * 2;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&8000u32.to_le_bytes());
    w.extend_from_slice(&16000u32.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend(std::iter::repeat(0u8).take(data_len as usize));
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_png_and_wav() {
        let png = B64.decode(PROBE_IMAGE_B64).unwrap();
        assert_eq!(kind_from_magic(&png), Some(MediaKind::Image));
        assert_eq!(kind_from_ext("png"), Some(MediaKind::Image));
        assert_eq!(kind_from_ext("mp4"), Some(MediaKind::Video));
        assert_eq!(kind_from_ext("wav"), Some(MediaKind::Audio));
        let wav = silence_wav();
        assert_eq!(kind_from_magic(&wav), Some(MediaKind::Audio));
        assert!(wav.starts_with(b"RIFF"));
    }

    #[test]
    fn data_uri_roundtrip() {
        let bytes = b"hello";
        let uri = data_uri("text/plain", bytes);
        let (mime, out) = decode_data_uri(&uri).unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(out, bytes);
    }

    #[test]
    fn size_cap_constant_matches_qwenpaw() {
        assert_eq!(MAX_INLINE_MEDIA_BYTES, 2 * 1024 * 1024);
    }

    #[test]
    fn attach_image_is_optimistic() {
        assert!(MediaCaps::default().attach_image());
        let mut off = MediaCaps::default();
        off.image = Some(false);
        assert!(!off.attach_image());
        assert!(!MediaCaps::default().attach_video());
        assert!(!MediaCaps::default().attach_audio());
    }

    #[test]
    fn color_keywords() {
        assert!(image_probe_hit("Red", ""));
        assert!(image_probe_hit("it's 红色", ""));
        assert!(!image_probe_hit("blue square", ""));
        assert!(video_probe_hit("navy", "", false));
        assert!(video_probe_hit("something", "", true));
        assert!(!video_probe_hit("", "", true));
    }

    #[test]
    fn api_image_has_image_url_key() {
        let p = MediaPart::image_url("data:image/png;base64,xx");
        let v = p.to_api_value();
        assert!(v.get("image_url").is_some());
        let j = p.to_jinja_value();
        assert!(j.get("image_url").is_some());
    }

    #[test]
    fn api_video_has_video_key_for_jinja() {
        let p = MediaPart::video_url("https://example.com/a.mp4");
        let api = p.to_api_value();
        assert_eq!(api["type"], "video");
        assert!(api.get("video").is_some(), "{api}");
        let j = p.to_jinja_value();
        assert_eq!(j["type"], "video");
        assert!(j.get("video").is_some());
    }

    #[test]
    fn media_bins_none_is_empty() {
        let b = MediaBins::none();
        assert!(b.ffmpeg.is_none());
        assert!(b.whisper.is_none());
        assert!(b.whisper_model.is_none());
    }

    #[test]
    fn bin_candidates_include_exe_on_windows() {
        let dir = PathBuf::from("bin");
        let c = bin_candidates(&dir, "ffmpeg");
        assert!(c.contains(&dir.join("ffmpeg")));
        #[cfg(windows)]
        {
            assert!(c
                .iter()
                .any(|p| p.extension().and_then(|e| e.to_str()) == Some("exe")
                    || p.extension().and_then(|e| e.to_str()) == Some("EXE")));
        }
    }

    #[test]
    fn host_image_result_decodes_raw_b64() {
        let (mime, bytes) = decode_image_payload(PROBE_IMAGE_B64).unwrap();
        assert_eq!(mime, "image/png");
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        let uri = data_uri("image/png", &bytes);
        let (m2, b2) = decode_image_payload(&uri).unwrap();
        assert_eq!(m2, "image/png");
        assert_eq!(b2, bytes);
    }

    #[test]
    fn persist_and_inline_generated_image() {
        let dir = std::env::temp_dir().join(format!("hyper-img-{}", uuid::Uuid::new_v4().simple()));
        let bytes = B64.decode(PROBE_IMAGE_B64).unwrap();
        let rel = persist_image_file(&dir, &bytes, "image/png").unwrap();
        assert!(rel.starts_with(".grok-hyper/generated/imagine-"));
        assert!(dir.join(&rel).is_file());
        let mut msg = crate::template::ChatMessage::assistant("ok");
        msg.parts = vec![MediaPart::image_url(rel)];
        inline_workspace_media(&dir, std::slice::from_mut(&mut msg), 1024 * 1024);
        assert!(msg.parts[0].url.starts_with("data:image/png;base64,"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mentions_image_needs_a_pointing_phrase() {
        assert!(!mentions_image("write a ppt skill"));
        assert!(!mentions_image("imagine a better layout"));
        assert!(!mentions_image("draw a logo"));
        assert!(mentions_image("look at this image"));
        assert!(mentions_image("把这张图调亮一点"));
        assert!(mentions_image("see foo.png"));
        assert!(mentions_image("the logo is too dark"));
    }

    #[test]
    fn retain_drops_prior_generated_image_unless_pointed_at() {
        let mut shot = crate::template::ChatMessage::assistant("");
        shot.parts = vec![MediaPart::image_url("data:image/jpeg;base64,yy")];
        let mut msgs = vec![
            crate::template::ChatMessage::user("draw a logo"),
            shot.clone(),
            crate::template::ChatMessage::user("write a ppt skill"),
        ];
        retain_referenced_media(&mut msgs);
        assert!(msgs.iter().all(|m| m.parts.is_empty()), "{msgs:?}");

        let mut pointed = vec![
            crate::template::ChatMessage::user("draw a logo"),
            shot,
            crate::template::ChatMessage::user("把这张图调亮一点"),
        ];
        retain_referenced_media(&mut pointed);
        assert_eq!(pointed[1].parts.len(), 1);
    }

    #[test]
    fn retain_keeps_current_turn_attachment_and_view() {
        let mut prior = crate::template::ChatMessage::assistant("");
        prior.parts = vec![MediaPart::image_url("data:image/jpeg;base64,old")];
        let mut user = crate::template::ChatMessage::user("what color is this file");
        user.parts = vec![MediaPart::image_url("data:image/png;base64,att")];
        let mut view = crate::template::ChatMessage::tool("c1", "Image loaded");
        view.parts = vec![MediaPart::image_url("data:image/png;base64,view")];
        let mut msgs = vec![prior, user, view];
        retain_referenced_media(&mut msgs);
        assert!(msgs[0].parts.is_empty(), "historical shot must drop");
        assert_eq!(msgs[1].parts.len(), 1);
        assert_eq!(msgs[2].parts.len(), 1);
    }
}
