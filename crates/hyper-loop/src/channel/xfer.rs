//! Load / classify / harvest media for channel inbound and outbound.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::media::{data_uri, decode_data_uri, MAX_INLINE_MEDIA_BYTES};

use super::envelope::ContentPart;
use super::outbound::reply_text;

/// Hard cap for a single download or outbound upload (16 MiB).
pub const FETCH_CAP: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Audio,
    Video,
    File,
}

#[derive(Clone, Debug)]
pub struct Blob {
    pub kind: Kind,
    pub mime: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Blob {
    pub fn to_part(&self) -> ContentPart {
        let src = if self.bytes.len() <= MAX_INLINE_MEDIA_BYTES && self.kind != Kind::File {
            data_uri(&self.mime, &self.bytes)
        } else {
            String::new()
        };
        match self.kind {
            Kind::Image => ContentPart::Image {
                image_url: src,
                url: String::new(),
                mime: self.mime.clone(),
            },
            Kind::Audio => ContentPart::Audio {
                audio_url: src,
                url: String::new(),
                mime: self.mime.clone(),
            },
            Kind::Video => ContentPart::Video {
                video_url: src,
                url: String::new(),
                mime: self.mime.clone(),
            },
            Kind::File => ContentPart::File {
                file_url: src,
                file_id: String::new(),
                name: self.name.clone(),
            },
        }
    }

    pub fn to_part_with_path(&self, path: &Path) -> ContentPart {
        let loc = path.display().to_string();
        match self.kind {
            Kind::Image => ContentPart::Image {
                image_url: loc.clone(),
                url: loc,
                mime: self.mime.clone(),
            },
            Kind::Audio => ContentPart::Audio {
                audio_url: loc.clone(),
                url: loc,
                mime: self.mime.clone(),
            },
            Kind::Video => ContentPart::Video {
                video_url: loc.clone(),
                url: loc,
                mime: self.mime.clone(),
            },
            Kind::File => ContentPart::File {
                file_url: loc,
                file_id: String::new(),
                name: self.name.clone(),
            },
        }
    }
}

pub fn kind_from_mime_name(mime: &str, name: &str) -> Kind {
    let m = mime.to_ascii_lowercase();
    if m.starts_with("image/") {
        return Kind::Image;
    }
    if m.starts_with("audio/") {
        return Kind::Audio;
    }
    if m.starts_with("video/") {
        return Kind::Video;
    }
    kind_from_name(name)
}

pub fn kind_from_name(name: &str) -> Kind {
    let n = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ext = n
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "svg" => {
            Kind::Image
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" => Kind::Audio,
        "mp4" | "webm" | "mov" | "mkv" | "avi" | "mpeg" | "mpg" => Kind::Video,
        _ => Kind::File,
    }
}

pub fn guess_mime(name: &str, kind: Kind) -> &'static str {
    let n = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ext = n
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "aac" => "audio/mp4",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "zip" => "application/zip",
        _ => match kind {
            Kind::Image => "image/jpeg",
            Kind::Audio => "audio/mpeg",
            Kind::Video => "video/mp4",
            Kind::File => "application/octet-stream",
        },
    }
}

pub fn is_sendable_rel(rel: &str) -> bool {
    let n = rel.replace('\\', "/").to_ascii_lowercase();
    if n.contains("/.git/")
        || n.starts_with(".git/")
        || n.contains("/node_modules/")
        || n.starts_with("node_modules/")
        || n.contains("/__pycache__/")
        || n.starts_with("__pycache__/")
    {
        return false;
    }
    if n.contains("/.grok-hyper/generated/") {
        return is_media_name(&n);
    }
    if crate::out_dir::is_out_rel(rel) {
        return is_product_name(&n) || is_media_name(&n);
    }
    is_media_name(&n)
}

fn is_media_name(n: &str) -> bool {
    matches!(kind_from_name(n), Kind::Image | Kind::Audio | Kind::Video)
}

fn is_product_name(n: &str) -> bool {
    let ext = n
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "svg"
            | "bmp"
            | "pdf"
            | "docx"
            | "doc"
            | "pptx"
            | "ppt"
            | "xlsx"
            | "xls"
            | "csv"
            | "html"
            | "htm"
            | "zip"
            | "md"
            | "txt"
            | "mp3"
            | "mp4"
            | "wav"
            | "webm"
    )
}

/// Markdown `![](src)` and sendable `[label](src.ext)` refs.
pub fn harvest_markdown(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    push_md_images(text, &mut out);
    push_md_files(text, &mut out);
    out
}

fn push_md_images(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == b'!' && bytes[i + 1] == b'[' {
            if let Some(src) = md_paren_src(&text[i..]) {
                if !src.is_empty() {
                    out.push(src);
                }
            }
        }
        i += 1;
    }
}

fn push_md_files(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes.get(i.saturating_sub(1)) != Some(&b'!') {
            if let Some(src) = md_paren_src(&text[i..]) {
                if is_sendable_rel(&src) || looks_remote_media(&src) {
                    out.push(src);
                }
            }
        }
        i += 1;
    }
}

fn md_paren_src(slice: &str) -> Option<String> {
    let rb = slice.find(']')?;
    let rest = slice.get(rb + 1..)?;
    if !rest.starts_with('(') {
        return None;
    }
    let inner = rest.get(1..)?;
    let end = inner.find(')')?;
    let raw = inner[..end].trim();
    let src = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('"');
    if src.is_empty() {
        None
    } else {
        Some(src.to_string())
    }
}

fn looks_remote_media(src: &str) -> bool {
    let s = src.to_ascii_lowercase();
    (s.starts_with("http://") || s.starts_with("https://") || s.starts_with("data:"))
        && (is_media_name(&s)
            || s.contains("/photo")
            || s.contains("image")
            || s.contains(".cdn.")
            || s.starts_with("data:image"))
}

pub fn part_src(part: &ContentPart) -> Option<&str> {
    match part {
        ContentPart::Image { image_url, url, .. } => nonempty(image_url).or_else(|| nonempty(url)),
        ContentPart::Audio { audio_url, url, .. } => nonempty(audio_url).or_else(|| nonempty(url)),
        ContentPart::Video { video_url, url, .. } => nonempty(video_url).or_else(|| nonempty(url)),
        ContentPart::File { file_url, .. } => nonempty(file_url),
        ContentPart::Text { .. } => None,
    }
}

pub fn http_src(part: &ContentPart) -> Option<&str> {
    let s = part_src(part)?;
    if s.starts_with("http://") || s.starts_with("https://") {
        Some(s)
    } else {
        None
    }
}

pub fn is_media_stub(text: &str) -> bool {
    let t = text.trim();
    matches!(t, "[图片]" | "[语音]" | "[视频]" | "[表情]") || t.starts_with("[文件]")
}

pub fn splice_media(parts: &mut Vec<ContentPart>, media: ContentPart) {
    parts.retain(|p| !p.as_text().is_some_and(is_media_stub));
    parts.push(media);
}

pub fn query_text_of(parts: &[ContentPart]) -> String {
    super::envelope::NativePayload {
        content_parts: parts.to_vec(),
        ..Default::default()
    }
    .query_text()
}

/// Pull http(s) media into an inline data URI or `~/.grok-hyper/inbox/` file.
pub async fn hydrate_http_parts(parts: &mut Vec<ContentPart>) {
    for p in parts.iter_mut() {
        let Some(src) = http_src(p).map(str::to_string) else {
            continue;
        };
        match load_src(&src, None).await {
            Ok(blob) => {
                if let Ok(got) = blob_to_inbound_part(blob) {
                    *p = got;
                }
            }
            Err(_) => {}
        }
    }
}

pub fn bytes_part(blob: &Blob) -> reqwest::multipart::Part {
    match reqwest::multipart::Part::bytes(blob.bytes.clone())
        .file_name(blob.name.clone())
        .mime_str(&blob.mime)
    {
        Ok(p) => p,
        Err(_) => reqwest::multipart::Part::bytes(blob.bytes.clone()).file_name(blob.name.clone()),
    }
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

#[allow(dead_code)]
pub fn is_media_part(part: &ContentPart) -> bool {
    !matches!(part, ContentPart::Text { .. })
}

/// Spoken caption only — adapters send Image/File/Audio/Video themselves.
pub fn spoken_text(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for p in parts {
        if let Some(t) = p.as_text() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        }
    }
    text
}

pub async fn load_src(src: &str, workspace: Option<&Path>) -> Result<Blob> {
    let src = src.trim();
    if src.is_empty() {
        return Err(Error::msg("empty media src"));
    }
    if let Some((mime, bytes)) = decode_data_uri(src) {
        if bytes.len() > FETCH_CAP {
            return Err(Error::msg("media over fetch cap"));
        }
        let kind = kind_from_mime_name(&mime, "file");
        let name = default_name(kind, &mime);
        return Ok(Blob {
            kind,
            mime,
            name,
            bytes,
        });
    }
    if src.starts_with("http://") || src.starts_with("https://") {
        return fetch_http(src).await;
    }
    let path = resolve_local(src, workspace)?;
    let bytes = std::fs::read(&path)?;
    if bytes.len() > FETCH_CAP {
        return Err(Error::msg("media over fetch cap"));
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let kind = kind_from_name(&name);
    Ok(Blob {
        mime: guess_mime(&name, kind).into(),
        kind,
        name,
        bytes,
    })
}

pub async fn load_part(part: &ContentPart, workspace: Option<&Path>) -> Result<Blob> {
    let src = part_src(part).ok_or_else(|| Error::msg("media part has no url"))?;
    let mut blob = load_src(src, workspace).await?;
    if let ContentPart::File { name, .. } = part {
        if !name.trim().is_empty() {
            blob.name = name.clone();
            blob.kind = kind_from_name(name);
            if blob.mime == "application/octet-stream" || blob.mime.starts_with("image/") {
                blob.mime = guess_mime(name, blob.kind).into();
            }
        }
    }
    if let ContentPart::Image { mime, .. }
    | ContentPart::Audio { mime, .. }
    | ContentPart::Video { mime, .. } = part
    {
        if !mime.is_empty() {
            blob.mime = mime.clone();
            blob.kind = kind_from_mime_name(mime, &blob.name);
        }
    }
    Ok(blob)
}

async fn fetch_http(url: &str) -> Result<Blob> {
    let resp = crate::llm_http::env_aware_client(30, url)?
        .get(url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!("media GET {}", resp.status())));
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let bytes = resp.bytes().await?;
    if bytes.len() > FETCH_CAP {
        return Err(Error::msg("media over fetch cap"));
    }
    let name = url
        .rsplit(['/', '?'])
        .find(|s| !s.is_empty() && s.contains('.'))
        .unwrap_or("file")
        .to_string();
    let kind = if mime.is_empty() {
        kind_from_name(&name)
    } else {
        kind_from_mime_name(&mime, &name)
    };
    let mime = if mime.is_empty() {
        guess_mime(&name, kind).to_string()
    } else {
        mime
    };
    Ok(Blob {
        kind,
        mime,
        name,
        bytes: bytes.to_vec(),
    })
}

fn resolve_local(src: &str, workspace: Option<&Path>) -> Result<PathBuf> {
    let p = Path::new(src);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if let Some(root) = workspace {
        return Ok(root.join(p));
    }
    Ok(std::env::current_dir()?.join(p))
}

fn default_name(kind: Kind, mime: &str) -> String {
    match kind {
        Kind::Image => {
            if mime.contains("png") {
                "image.png".into()
            } else if mime.contains("gif") {
                "image.gif".into()
            } else if mime.contains("webp") {
                "image.webp".into()
            } else {
                "image.jpg".into()
            }
        }
        Kind::Audio => "audio.ogg".into(),
        Kind::Video => "video.mp4".into(),
        Kind::File => "file.bin".into(),
    }
}

pub fn save_inbox(bytes: &[u8], name: &str) -> Result<PathBuf> {
    let home = crate::config::Config::home_dir().map_err(Error::msg)?;
    let dir = home.join("inbox");
    std::fs::create_dir_all(&dir)?;
    let safe = sanitize_name(name);
    let path = dir.join(format!("{}-{safe}", &uuid::Uuid::new_v4().to_string()[..8]));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

pub fn blob_to_inbound_part(blob: Blob) -> Result<ContentPart> {
    if blob.bytes.len() <= MAX_INLINE_MEDIA_BYTES && blob.kind != Kind::File {
        return Ok(blob.to_part());
    }
    let path = save_inbox(&blob.bytes, &blob.name)?;
    Ok(blob.to_part_with_path(&path))
}

fn sanitize_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if base.is_empty() || base == "." || base == ".." {
        "file.bin".into()
    } else {
        base
    }
}

/// Turn spoken answer + files the agent wrote into outbound `content_parts`.
pub fn reply_parts(text: &str, workspace: &Path, written: &[String]) -> Vec<ContentPart> {
    let mut refs: Vec<String> = harvest_markdown(text);
    for w in written {
        if is_sendable_rel(w) {
            refs.push(w.clone());
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut media = Vec::new();
    for src in refs {
        let key = src.replace('\\', "/");
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(part) = src_to_part(&src, workspace) {
            media.push(part);
        }
    }
    let caption = strip_attached_markdown(text, &media);
    let mut parts = if caption.trim().is_empty() {
        Vec::new()
    } else {
        reply_text(caption)
    };
    parts.extend(media);
    parts
}

fn src_to_part(src: &str, workspace: &Path) -> Option<ContentPart> {
    if src.starts_with("data:") {
        let (mime, bytes) = decode_data_uri(src)?;
        if bytes.len() > FETCH_CAP {
            return None;
        }
        let kind = kind_from_mime_name(&mime, "file");
        return Some(
            Blob {
                kind,
                mime,
                name: default_name(kind, ""),
                bytes,
            }
            .to_part(),
        );
    }
    if src.starts_with("http://") || src.starts_with("https://") {
        let kind = kind_from_name(src);
        let mime = guess_mime(src, kind);
        return Some(match kind {
            Kind::Image => ContentPart::Image {
                image_url: src.to_string(),
                url: String::new(),
                mime: mime.into(),
            },
            Kind::Audio => ContentPart::Audio {
                audio_url: src.to_string(),
                url: String::new(),
                mime: mime.into(),
            },
            Kind::Video => ContentPart::Video {
                video_url: src.to_string(),
                url: String::new(),
                mime: mime.into(),
            },
            Kind::File => ContentPart::File {
                file_url: src.to_string(),
                file_id: String::new(),
                name: src.rsplit('/').next().unwrap_or("file").to_string(),
            },
        });
    }
    let path = resolve_local(src, Some(workspace)).ok()?;
    if !path.is_file() {
        return None;
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let kind = kind_from_name(&name);
    let loc = path.display().to_string();
    let mime = guess_mime(&name, kind);
    Some(match kind {
        Kind::Image => ContentPart::Image {
            image_url: loc.clone(),
            url: loc,
            mime: mime.into(),
        },
        Kind::Audio => ContentPart::Audio {
            audio_url: loc.clone(),
            url: loc,
            mime: mime.into(),
        },
        Kind::Video => ContentPart::Video {
            video_url: loc.clone(),
            url: loc,
            mime: mime.into(),
        },
        Kind::File => ContentPart::File {
            file_url: loc,
            file_id: String::new(),
            name,
        },
    })
}

fn strip_attached_markdown(text: &str, media: &[ContentPart]) -> String {
    let mut s = text.to_string();
    for part in media {
        if let Some(src) = part_src(part) {
            s = strip_one_md(&s, src);
            if let Some(name) = Path::new(src).file_name().and_then(|n| n.to_str()) {
                s = strip_one_md(&s, name);
            }
        }
    }
    s.trim().to_string()
}

fn strip_one_md(text: &str, src: &str) -> String {
    let mut out = text.to_string();
    for needle in [
        format!("![]({src})"),
        format!("![]({src} )"),
        format!("({src})"),
    ] {
        out = out.replace(&needle, "");
    }
    out
}

#[allow(dead_code)]
pub fn file_note(path: &Path, name: &str) -> ContentPart {
    let n = if name.is_empty() {
        path.file_name().and_then(|s| s.to_str()).unwrap_or("file")
    } else {
        name
    };
    ContentPart::text(format!("[文件] {n}（已保存 {}）", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harvest_images_and_docx() {
        let t = "see ![shot](out/a.png) and [guide](out/guide.docx) plus ![x](https://cdn.example/p.jpg)";
        let h = harvest_markdown(t);
        assert!(h.iter().any(|s| s.ends_with("a.png")));
        assert!(h.iter().any(|s| s.ends_with("guide.docx")));
        assert!(h.iter().any(|s| s.contains("cdn.example")));
    }

    #[test]
    fn sendable_out_and_media() {
        assert!(is_sendable_rel("out/report.pdf"));
        assert!(is_sendable_rel("out/pic.png"));
        assert!(is_sendable_rel("photo.jpg"));
        assert!(!is_sendable_rel("src/main.rs"));
        assert!(!is_sendable_rel("node_modules/x.png"));
        assert!(is_sendable_rel(".grok-hyper/generated/1.png"));
    }

    #[test]
    fn reply_attaches_written_png() {
        let dir = std::env::temp_dir().join(format!("hyper-xfer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let img = dir.join("out/hi.png");
        std::fs::write(&img, [0x89, b'P', b'N', b'G', 0, 0, 0, 0]).unwrap();
        let parts = reply_parts("saved it", &dir, &["out/hi.png".into()]);
        assert!(parts.iter().any(|p| matches!(p, ContentPart::Image { .. })));
        assert!(parts.iter().any(|p| p.as_text() == Some("saved it")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spoken_skips_media_fallback() {
        let parts = vec![
            ContentPart::text("hi"),
            ContentPart::Image {
                image_url: "https://x/a.png".into(),
                url: String::new(),
                mime: String::new(),
            },
        ];
        assert_eq!(spoken_text(&parts), "hi");
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_from_name("a.PNG"), Kind::Image);
        assert_eq!(kind_from_name("notes.pdf"), Kind::File);
    }

    #[test]
    fn http_src_only_remote() {
        let remote = ContentPart::Image {
            image_url: "https://cdn.example/p.jpg".into(),
            url: String::new(),
            mime: String::new(),
        };
        let local = ContentPart::Image {
            image_url: "/tmp/p.jpg".into(),
            url: String::new(),
            mime: String::new(),
        };
        assert_eq!(http_src(&remote), Some("https://cdn.example/p.jpg"));
        assert!(http_src(&local).is_none());
    }

    #[test]
    fn splice_drops_stubs() {
        let mut parts = vec![ContentPart::text("see this"), ContentPart::text("[图片]")];
        splice_media(
            &mut parts,
            ContentPart::Image {
                image_url: "https://x/a.png".into(),
                url: String::new(),
                mime: "image/png".into(),
            },
        );
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].as_text(), Some("see this"));
        assert!(matches!(parts[1], ContentPart::Image { .. }));
    }
}
