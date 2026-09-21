//! Cross-platform `ffmpeg` stills and `whisper-cli` transcripts.
//!
//! Spawn via `Command` (no shell). On Windows, hide the console window. Seek
//! `-ss` before `-i` so a long clip is not decoded. Do not assume Homebrew.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::media::{MediaBins, MediaKind, MediaPart, MAX_INLINE_MEDIA_BYTES};

pub const FRAME_COUNT: usize = 3;
pub const VIDEO_FETCH_MAX_BYTES: usize = 12 * 1024 * 1024;
pub const AUDIO_FETCH_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const AUDIO_MAX_SECS: u32 = 60;
pub const TRANSCRIPT_MAX_CHARS: usize = 4000;

const FRAME_MAX_SIDE: u32 = 768;
const FRAME_JPEG_Q: &str = "5";
const FRAME_MAX_BYTES: usize = 400_000;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(20);
const DURATION_TIMEOUT: Duration = Duration::from_secs(8);
const WHISPER_TIMEOUT: Duration = Duration::from_secs(120);
const PROC_PIPE_CAP: usize = 1024 * 1024;

struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct SampledVideo {
    pub parts: Vec<MediaPart>,
    pub label: String,
}

pub fn sample_times(duration: f64, n: usize) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    if duration <= 0.0 {
        return (0..n).map(|i| i as f64 * 0.5).collect();
    }
    if duration < 0.4 {
        return vec![(duration * 0.5).max(0.0)];
    }
    let n = n.min(3);
    let fracs: &[f64] = match n {
        1 => &[0.50],
        2 => &[0.20, 0.80],
        _ => &[0.10, 0.50, 0.90],
    };
    fracs.iter().take(n).map(|f| duration * f).collect()
}

pub fn stamp_label(times: &[f64], duration: f64) -> String {
    if times.is_empty() {
        return "no stills".into();
    }
    if duration > 0.4 && (times.len() == 3) {
        let near = |t: f64, f: f64| (t - duration * f).abs() < 0.05_f64.max(duration * 0.02);
        if near(times[0], 0.10) && near(times[1], 0.50) && near(times[2], 0.90) {
            return "10%/50%/90%".into();
        }
    }
    times
        .iter()
        .map(|t| format!("{t:.2}s"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn parse_ffmpeg_duration(stderr: &str) -> Option<f64> {
    let idx = stderr.find("Duration:")?;
    let rest = stderr[idx + "Duration:".len()..].trim_start();
    let token = rest.split([',', ' ', '\t']).find(|s| !s.is_empty())?;
    if token.eq_ignore_ascii_case("N/A") {
        return None;
    }
    parse_hms(token)
}

fn parse_hms(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(h * 3600.0 + m * 60.0 + sec)
}

pub fn clip_transcript(s: &str) -> String {
    let t = s.trim();
    let count = t.chars().count();
    if count <= TRANSCRIPT_MAX_CHARS {
        t.to_string()
    } else {
        format!(
            "{}…",
            t.chars().take(TRANSCRIPT_MAX_CHARS).collect::<String>()
        )
    }
}

pub fn no_watch_msg(name: &str, why: &str) -> String {
    format!("Cannot watch {name}: {why}.")
}

pub fn no_hear_msg(name: &str, why: &str) -> String {
    format!("Cannot hear {name}: {why}.")
}

pub async fn extract_frames(bins: &MediaBins, path: &Path) -> Result<SampledVideo, String> {
    if crate::tools::is_special_file(path) {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let ffmpeg = bins
        .ffmpeg
        .as_ref()
        .ok_or_else(|| "ffmpeg is not on PATH".to_string())?;
    let tmp = TmpDir(
        std::env::temp_dir().join(format!("hyper-frames-{}", uuid::Uuid::new_v4().simple())),
    );
    std::fs::create_dir_all(&tmp.0).map_err(|e| format!("frame dir: {e}"))?;
    let duration = probe_duration(bins, path).await.unwrap_or(0.0);
    let stamps = sample_times(duration, FRAME_COUNT);
    let mut parts = Vec::new();
    let mut kept = Vec::new();
    for (i, t) in stamps.iter().enumerate() {
        let out = tmp.0.join(format!("f{i}.jpg"));
        if grab_frame(ffmpeg, path, *t, &out).await && accept_jpeg(&out) {
            if let Ok(bytes) = crate::tools::read_bytes_regular(&out) {
                parts.push(MediaPart::data_uri(MediaKind::Image, "image/jpeg", &bytes));
                kept.push(*t);
            }
        }
    }
    if parts.is_empty() {
        return Err("ffmpeg produced no stills".into());
    }
    Ok(SampledVideo {
        label: stamp_label(&kept, duration),
        parts,
    })
}

async fn grab_frame(ffmpeg: &Path, input: &Path, t: f64, out: &Path) -> bool {
    let vf = format!("scale=min(iw\\,{FRAME_MAX_SIDE}):-2");
    let mut cmd = media_cmd(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-ss")
        .arg(format!("{t:.3}"))
        .arg("-i")
        .arg(input)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(&vf)
        .arg("-q:v")
        .arg(FRAME_JPEG_Q)
        .arg("-threads")
        .arg("1")
        .arg("-y")
        .arg(out);
    cmd.stdout(Stdio::null());
    match tokio::time::timeout(FFMPEG_TIMEOUT, cmd.status()).await {
        Ok(Ok(st)) if st.success() && out.is_file() => true,
        _ => {
            // Older ffmpeg builds may reject the scale expression; try a still without scale.
            let mut cmd = media_cmd(ffmpeg);
            cmd.arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg("-nostdin")
                .arg("-ss")
                .arg(format!("{t:.3}"))
                .arg("-i")
                .arg(input)
                .arg("-frames:v")
                .arg("1")
                .arg("-q:v")
                .arg(FRAME_JPEG_Q)
                .arg("-y")
                .arg(out);
            cmd.stdout(Stdio::null());
            matches!(
                tokio::time::timeout(FFMPEG_TIMEOUT, cmd.status()).await,
                Ok(Ok(st)) if st.success() && out.is_file()
            )
        }
    }
}

fn accept_jpeg(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => {
            let n = m.len() as usize;
            n > 32 && n <= FRAME_MAX_BYTES.min(MAX_INLINE_MEDIA_BYTES)
        }
        Err(_) => false,
    }
}

async fn probe_duration(bins: &MediaBins, path: &Path) -> Option<f64> {
    if let Some(ffprobe) = bins.ffprobe.as_ref() {
        let mut cmd = media_cmd(ffprobe);
        cmd.arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("csv=p=0")
            .arg(path);
        if let Ok((ok, stdout, _)) = run_capped(&mut cmd, DURATION_TIMEOUT, PROC_PIPE_CAP).await {
            if ok {
                if let Ok(s) = String::from_utf8(stdout) {
                    if let Ok(d) = s.trim().parse::<f64>() {
                        if d.is_finite() && d > 0.0 {
                            return Some(d);
                        }
                    }
                }
            }
        }
    }
    let ffmpeg = bins.ffmpeg.as_ref()?;
    let mut cmd = media_cmd(ffmpeg);
    cmd.arg("-hide_banner").arg("-nostdin").arg("-i").arg(path);
    let out = run_capped(&mut cmd, DURATION_TIMEOUT, PROC_PIPE_CAP)
        .await
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.2);
    parse_ffmpeg_duration(&stderr)
}

/// Decode up to [`AUDIO_MAX_SECS`] of audio to 16 kHz mono WAV, then whisper-cli.
pub async fn transcribe_file(bins: &MediaBins, path: &Path) -> Result<String, String> {
    if crate::tools::is_special_file(path) {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let whisper = bins
        .whisper
        .as_ref()
        .ok_or_else(|| "whisper-cli is not on PATH".to_string())?;
    let model = match bins.whisper_model.as_ref() {
        Some(m) if m.is_file() => m,
        _ => {
            return Err(format!(
                "whisper.cpp model not found (expected {})",
                MediaBins::expected_whisper_model().display()
            ));
        }
    };
    let tmp =
        TmpDir(std::env::temp_dir().join(format!("hyper-asr-{}", uuid::Uuid::new_v4().simple())));
    std::fs::create_dir_all(&tmp.0).map_err(|e| format!("asr dir: {e}"))?;
    let wav = tmp.0.join("in.wav");
    to_wav16k(bins, path, &wav).await?;
    let prefix = tmp.0.join("out");
    let text = run_whisper(whisper, model, &wav, &prefix).await?;
    Ok(clip_transcript(&text))
}

async fn to_wav16k(bins: &MediaBins, input: &Path, wav: &Path) -> Result<(), String> {
    if crate::tools::is_special_file(input) {
        return Err(format!("{} is not a regular file", input.display()));
    }
    if let Some(ffmpeg) = bins.ffmpeg.as_ref() {
        let mut cmd = media_cmd(ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-i")
            .arg(input)
            .arg("-t")
            .arg(AUDIO_MAX_SECS.to_string())
            .arg("-ac")
            .arg("1")
            .arg("-ar")
            .arg("16000")
            .arg("-vn")
            .arg("-f")
            .arg("wav")
            .arg(wav);
        cmd.stdout(Stdio::null());
        match tokio::time::timeout(FFMPEG_TIMEOUT, cmd.status()).await {
            Ok(Ok(st)) if st.success() && wav.is_file() => return Ok(()),
            Ok(Ok(st)) => {
                return Err(format!(
                    "ffmpeg could not decode audio (exit {})",
                    st.code().unwrap_or(-1)
                ));
            }
            _ => return Err("ffmpeg audio convert timed out".into()),
        }
    }
    if let Ok(meta) = std::fs::metadata(input) {
        if meta.len() as usize > AUDIO_FETCH_MAX_BYTES {
            return Err(format!(
                "audio is {} bytes and exceeds the {AUDIO_FETCH_MAX_BYTES}-byte limit",
                meta.len()
            ));
        }
    }
    // Already a WAV and no ffmpeg: pass through if it looks like RIFF/WAVE.
    let head = read_head(input, 12).unwrap_or_default();
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE" {
        std::fs::copy(input, wav).map_err(|e| format!("copy wav: {}", super::io_user_msg(&e)))?;
        return Ok(());
    }
    Err("ffmpeg is not on PATH (needed to decode audio)".into())
}

async fn run_whisper(
    whisper: &Path,
    model: &Path,
    wav: &Path,
    prefix: &Path,
) -> Result<String, String> {
    let attempts: [&[&str]; 4] = [
        &["-nt", "-np", "-sns", "-t", "2", "-l", "auto"],
        &["-nt", "-np", "-t", "2", "-l", "auto"],
        &["-nt", "-t", "2"],
        &[],
    ];
    let mut last = "whisper-cli failed".to_string();
    for extra in attempts {
        let mut cmd = media_cmd(whisper);
        cmd.arg("-m")
            .arg(model)
            .arg("-f")
            .arg(wav)
            .arg("-otxt")
            .arg("-of")
            .arg(prefix);
        for a in extra {
            cmd.arg(a);
        }
        match run_capped(&mut cmd, WHISPER_TIMEOUT, PROC_PIPE_CAP).await {
            Ok((ok, stdout, _)) => {
                let txt = prefix.with_extension("txt");
                if crate::tools::is_special_file(&txt) {
                    // skip
                } else if txt.is_file() {
                    let raw = crate::tools::read_text_if_regular(&txt).unwrap_or_default();
                    if !raw.trim().is_empty() || ok {
                        return Ok(clip_transcript(&raw));
                    }
                }
                let stdout = String::from_utf8_lossy(&stdout);
                let cleaned = clean_whisper_stdout(&stdout);
                if !cleaned.is_empty() {
                    return Ok(clip_transcript(&cleaned));
                }
                last = format!("whisper-cli exit {}", if ok { 0 } else { -1 });
            }
            Err(e) => last = format!("whisper-cli: {e}"),
        }
    }
    Err(last)
}

fn clean_whisper_stdout(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("whisper_")
                && !l.starts_with("system_info")
                && !l.starts_with("main:")
                && !l.starts_with("ggml_")
                && !l.starts_with("encoder:")
                && !l.starts_with("decoder:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = crate::tools::open_read_nonblock(path).ok()?;
    let mut buf = vec![0u8; n];
    match f.read(&mut buf) {
        Ok(got) => {
            buf.truncate(got);
            Some(buf)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(_) => None,
    }
}

fn media_cmd(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::proc_spawn::hide_window_async(&mut cmd);
    cmd
}

async fn read_capped_bytes<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    cap: usize,
) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    const DISCARD_MAX: usize = 8 * 1024 * 1024;
    let mut buf = Vec::new();
    let mut discarded = 0usize;
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&tmp[..n.min(room)]);
                    discarded = discarded.saturating_add(n.saturating_sub(room));
                } else {
                    discarded = discarded.saturating_add(n);
                }
                if discarded >= DISCARD_MAX {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf
}

async fn run_capped(
    cmd: &mut Command,
    timeout: Duration,
    cap: usize,
) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    let mut child = cmd.spawn().map_err(|e| super::io_user_msg(&e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(async move { read_capped_bytes(stdout, cap).await });
    let err_task = tokio::spawn(async move { read_capped_bytes(stderr, cap).await });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(st)) => st,
        Ok(Err(e)) => {
            let _ = child.start_kill();
            let _ = out_task.await;
            let _ = err_task.await;
            return Err(e.to_string());
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;
            return Err("timed out".into());
        }
    };
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

/// Encode a solid-color clip with lavfi (Windows / Linux / macOS). Used by tests.
#[cfg(test)]
pub async fn encode_color_mp4(
    ffmpeg: &Path,
    out: &Path,
    color: &str,
    seconds: f64,
) -> Result<(), String> {
    let mut cmd = media_cmd(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("color=c={color}:s=64x64:d={seconds}:r=10"))
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg(out);
    cmd.stdout(Stdio::null());
    match tokio::time::timeout(FFMPEG_TIMEOUT, cmd.status()).await {
        Ok(Ok(st)) if st.success() && out.is_file() => Ok(()),
        Ok(Ok(st)) => Err(format!(
            "ffmpeg lavfi failed (exit {})",
            st.code().unwrap_or(-1)
        )),
        _ => Err("ffmpeg lavfi timed out".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_skip_frame_zero() {
        let t = sample_times(10.0, 3);
        assert_eq!(t.len(), 3);
        assert!((t[0] - 1.0).abs() < 1e-9);
        assert!((t[1] - 5.0).abs() < 1e-9);
        assert!((t[2] - 9.0).abs() < 1e-9);
        assert_eq!(stamp_label(&t, 10.0), "10%/50%/90%");
    }

    #[test]
    fn clip_transcript_respects_max_chars() {
        let s = "a".repeat(TRANSCRIPT_MAX_CHARS + 8);
        let clipped = clip_transcript(&s);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), TRANSCRIPT_MAX_CHARS + 1);
    }

    #[test]
    fn short_clip_one_midpoint() {
        let t = sample_times(0.2, 3);
        assert_eq!(t.len(), 1);
        assert!((t[0] - 0.1).abs() < 1e-9);
    }

    #[test]
    fn parse_duration_from_ffmpeg_banner() {
        let s = "Input #0, mov,mp4:\n  Duration: 00:00:01.20, start: 0.000000, bitrate: 32 kb/s\n";
        assert!((parse_ffmpeg_duration(s).unwrap() - 1.2).abs() < 1e-6);
        assert!(parse_ffmpeg_duration("Duration: N/A, start: 0").is_none());
    }

    #[test]
    fn transcript_clip_keeps_short() {
        assert_eq!(clip_transcript("  hi  "), "hi");
        let long: String = "x".repeat(TRANSCRIPT_MAX_CHARS + 10);
        let c = clip_transcript(&long);
        assert!(c.ends_with('…'));
        assert_eq!(c.chars().count(), TRANSCRIPT_MAX_CHARS + 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_frames_fifo_is_error_not_hang() {
        let dir = std::env::temp_dir().join(format!("hyper-frames-fifo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("clip.mp4");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let err = match extract_frames(&crate::media::MediaBins::none(), &fifo).await {
            Err(e) => e,
            Ok(_) => panic!("FIFO video must not extract"),
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO video must not block ffmpeg: {:?}",
            started.elapsed()
        );
        assert!(err.contains("not a regular file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
