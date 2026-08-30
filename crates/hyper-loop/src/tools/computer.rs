//! `ComputerUse` — screenshot + mouse/keyboard on the machine running Hyper.
//!
//! Extra tool (not in the core Cursor-shaped set). Coordinates are in the last
//! screenshot's pixel space (top-left origin). macOS needs Screen Recording
//! (screenshot) and Accessibility (input). Windows needs an interactive
//! desktop session. Linux returns a clear error: not supported.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use image::{DynamicImage, GenericImageView, RgbaImage};
use serde_json::Value;

use crate::media::{MediaKind, MediaPart, MAX_INLINE_MEDIA_BYTES};
use crate::tool_calls::{CancelFlag, ToolCall, ToolResponse, ToolState};

const MAX_EDGE: u32 = 1280;
const MAX_WAIT_MS: u32 = 8_000;
const WAIT_SLICE: Duration = Duration::from_millis(50);
#[allow(dead_code)]
const MAX_TYPE_CHARS: usize = 4_000;
#[allow(dead_code)]
const SETTLE_MS: u64 = 40;

static LAST_SHOT: OnceLock<Mutex<HashMap<String, ShotMeta>>> = OnceLock::new();

thread_local! {
    static SHOT_SESSION: RefCell<String> = const { RefCell::new(String::new()) };
}

fn shot_map() -> &'static Mutex<HashMap<String, ShotMeta>> {
    LAST_SHOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_shot_session(id: &str) {
    SHOT_SESSION.with(|s| s.borrow_mut().clone_from(&id.to_string()));
}

fn shot_session() -> String {
    SHOT_SESSION.with(|s| s.borrow().clone())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ShotMeta {
    pub origin_x: i32,
    pub origin_y: i32,
    pub screen_w: u32,
    pub screen_h: u32,
    pub img_w: u32,
    pub img_h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Screenshot,
    ListDisplays,
    Click,
    DoubleClick,
    RightClick,
    Move,
    Drag,
    Scroll,
    Type,
    Key,
    Wait,
}

pub(crate) fn is_observe(args: &Value) -> bool {
    matches!(
        parse_action_name(args),
        Some(Action::Screenshot | Action::ListDisplays | Action::Wait)
    )
}

pub(crate) fn parse_action_name(args: &Value) -> Option<Action> {
    let raw = args
        .get("action")
        .or_else(|| args.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], "");
    Some(match raw.as_str() {
        "screenshot" | "screen" | "capture" => Action::Screenshot,
        "listdisplays" | "displays" | "monitors" => Action::ListDisplays,
        "click" | "leftclick" | "left" => Action::Click,
        "doubleclick" | "dblclick" => Action::DoubleClick,
        "rightclick" | "right" => Action::RightClick,
        "move" | "mousemove" | "hover" => Action::Move,
        "drag" | "dragto" => Action::Drag,
        "scroll" | "wheel" => Action::Scroll,
        "type" | "typewrite" | "text" => Action::Type,
        "key" | "hotkey" | "press" => Action::Key,
        "wait" | "sleep" => Action::Wait,
        _ => return None,
    })
}

pub(crate) fn image_to_screen(ix: f64, iy: f64, meta: &ShotMeta) -> (i32, i32) {
    let x = if meta.img_w == 0 {
        meta.origin_x
    } else {
        meta.origin_x + ((ix / meta.img_w as f64) * meta.screen_w as f64).round() as i32
    };
    let y = if meta.img_h == 0 {
        meta.origin_y
    } else {
        meta.origin_y + ((iy / meta.img_h as f64) * meta.screen_h as f64).round() as i32
    };
    let max_x = meta
        .origin_x
        .saturating_add(meta.screen_w.saturating_sub(1) as i32);
    let max_y = meta
        .origin_y
        .saturating_add(meta.screen_h.saturating_sub(1) as i32);
    (x.clamp(meta.origin_x, max_x), y.clamp(meta.origin_y, max_y))
}

pub fn os_hint(err: &str) -> String {
    let e = err.trim();
    #[cfg(target_os = "macos")]
    {
        format!(
            "Error: {e}. On macOS grant Screen Recording (screenshot) and Accessibility (click/type) to grok-hyper, Terminal, or this IDE in System Settings → Privacy & Security, then retry."
        )
    }
    #[cfg(target_os = "windows")]
    {
        format!(
            "Error: {e}. Run Hyper in the logged-in desktop session (not a Windows service or disconnected RDP)."
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        format!("Error: {e}. ComputerUse is supported on Windows and macOS; this host is not.")
    }
}

fn aborted(id: &str) -> ToolResponse {
    ToolResponse::text(id, "Error: tool task aborted", ToolState::Interrupted)
}

pub async fn computer_use(call: &ToolCall, cancel: CancelFlag, session_id: &str) -> ToolResponse {
    if cancel.is_cancelled() {
        return aborted(&call.id);
    }
    let owned = call.clone();
    let flag = cancel.clone();
    let sid = session_id.to_string();
    let mut join = tokio::task::spawn_blocking(move || {
        set_shot_session(&sid);
        execute_sync_cancel(&owned, &flag)
    });
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            join.abort();
            aborted(&call.id)
        }
        r = &mut join => match r {
            Ok(_) if cancel.is_cancelled() => aborted(&call.id),
            Ok(r) => r,
            Err(e) if e.is_cancelled() => aborted(&call.id),
            Err(e) => ToolResponse::text(
                &call.id,
                format!("Error: ComputerUse task failed: {e}"),
                ToolState::Error,
            ),
        }
    }
}

pub(crate) fn execute_sync(call: &ToolCall) -> ToolResponse {
    execute_sync_cancel(call, &CancelFlag::new())
}

fn execute_sync_cancel(call: &ToolCall, cancel: &CancelFlag) -> ToolResponse {
    if cancel.is_cancelled() {
        return aborted(&call.id);
    }
    let Some(action) = parse_action_name(&call.arguments) else {
        return ToolResponse::text(
            &call.id,
            "Error: ComputerUse needs action: screenshot, list_displays, click, double_click, right_click, move, drag, scroll, type, key, or wait.",
            ToolState::Error,
        );
    };
    match action {
        Action::Wait => run_wait(&call.id, &call.arguments, cancel),
        Action::ListDisplays => run_list_displays(&call.id),
        Action::Screenshot => run_screenshot(&call.id, &call.arguments, cancel),
        Action::Click
        | Action::DoubleClick
        | Action::RightClick
        | Action::Move
        | Action::Drag
        | Action::Scroll
        | Action::Type
        | Action::Key => run_input(&call.id, action, &call.arguments),
    }
}

fn run_wait(id: &str, args: &Value, cancel: &CancelFlag) -> ToolResponse {
    let ms = arg_u32(args, "ms")
        .or_else(|| arg_u32(args, "duration"))
        .unwrap_or(300);
    let ms = ms.min(MAX_WAIT_MS);
    let deadline = std::time::Instant::now() + Duration::from_millis(ms as u64);
    loop {
        if cancel.is_cancelled() {
            return aborted(id);
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return ToolResponse::text(id, format!("waited {ms}ms"), ToolState::Success);
        }
        std::thread::sleep(left.min(WAIT_SLICE));
    }
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().map(|v| v as u32).or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u32)
        }),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[allow(dead_code)]
fn remember(meta: ShotMeta) {
    crate::lock_unpoison(shot_map()).insert(shot_session(), meta);
}

#[allow(dead_code)]
fn last_shot() -> Option<ShotMeta> {
    crate::lock_unpoison(shot_map())
        .get(&shot_session())
        .copied()
}

#[allow(dead_code)]
fn resolve_point(args: &Value) -> Result<(i32, i32), String> {
    let ix = arg_f64(args, "x").ok_or_else(|| "Error: click/move needs numeric x.".to_string())?;
    let iy = arg_f64(args, "y").ok_or_else(|| "Error: click/move needs numeric y.".to_string())?;
    if let Some(meta) = last_shot() {
        Ok(image_to_screen(ix, iy, &meta))
    } else {
        Ok((ix.round() as i32, iy.round() as i32))
    }
}

pub(crate) fn encode_screenshot(
    rgba: RgbaImage,
    max_edge: u32,
    max_bytes: usize,
) -> Result<(Vec<u8>, u32, u32), String> {
    let src = DynamicImage::ImageRgba8(rgba);
    let (sw, sh) = src.dimensions();
    let long = sw.max(sh).max(1);
    let mut img = if long > max_edge {
        let s = max_edge as f32 / long as f32;
        src.resize(
            ((sw as f32) * s).round().max(1.0) as u32,
            ((sh as f32) * s).round().max(1.0) as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        src
    };
    for quality in [82, 70, 55, 40] {
        let bytes = jpeg_bytes(&img, quality)?;
        if bytes.len() <= max_bytes {
            return Ok((bytes, img.width(), img.height()));
        }
    }
    // Last resort: shrink until it fits.
    for _ in 0..4 {
        let w = (img.width() / 2).max(1);
        let h = (img.height() / 2).max(1);
        img = img.resize_exact(w, h, image::imageops::FilterType::Triangle);
        let bytes = jpeg_bytes(&img, 40)?;
        if bytes.len() <= max_bytes {
            return Ok((bytes, img.width(), img.height()));
        }
    }
    Err("Error: screenshot is still over the 2 MB inline cap after downscale.".into())
}

fn jpeg_bytes(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("Error: jpeg encode failed: {e}"))?;
    Ok(buf)
}

fn ok_media(id: &str, text: String, jpeg: Vec<u8>) -> ToolResponse {
    let mut r = ToolResponse::text(id, text, ToolState::Success);
    r.media = vec![MediaPart::data_uri(MediaKind::Image, "image/jpeg", &jpeg)];
    r
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_list_displays(id: &str) -> ToolResponse {
    match list_displays_text() {
        Ok(text) => ToolResponse::text(id, text, ToolState::Success),
        Err(e) => ToolResponse::text(id, os_hint(&e), ToolState::Error),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn run_list_displays(id: &str) -> ToolResponse {
    ToolResponse::text(id, os_hint("no displays"), ToolState::Error)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_screenshot(id: &str, args: &Value, cancel: &CancelFlag) -> ToolResponse {
    let display = arg_u32(args, "display").map(|v| v as i32);
    match capture_display(display) {
        Ok((rgba, mut meta, label)) => {
            let phys_w = rgba.width();
            let phys_h = rgba.height();
            match encode_screenshot(rgba, MAX_EDGE, MAX_INLINE_MEDIA_BYTES) {
                Ok((jpeg, img_w, img_h)) => {
                    if cancel.is_cancelled() {
                        return aborted(id);
                    }
                    meta.img_w = img_w;
                    meta.img_h = img_h;
                    remember(meta);
                    ok_media(
                        id,
                        format!(
                            "screenshot {label}: image {img_w}x{img_h} (display {phys_w}x{phys_h} at {},{}). Click x,y in this image; origin is top-left.",
                            meta.origin_x, meta.origin_y
                        ),
                        jpeg,
                    )
                }
                Err(e) => ToolResponse::text(id, e, ToolState::Error),
            }
        }
        Err(e) => ToolResponse::text(id, os_hint(&e), ToolState::Error),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn run_screenshot(id: &str, _args: &Value, _cancel: &CancelFlag) -> ToolResponse {
    ToolResponse::text(id, os_hint("no displays"), ToolState::Error)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_input(id: &str, action: Action, args: &Value) -> ToolResponse {
    match play_input(action, args) {
        Ok(text) => {
            std::thread::sleep(Duration::from_millis(SETTLE_MS));
            ToolResponse::text(id, text, ToolState::Success)
        }
        Err(e) if e.starts_with("Error:") => ToolResponse::text(id, e, ToolState::Error),
        Err(e) => ToolResponse::text(id, os_hint(&e), ToolState::Error),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn run_input(id: &str, _action: Action, _args: &Value) -> ToolResponse {
    ToolResponse::text(id, os_hint("mouse/keyboard unavailable"), ToolState::Error)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod sys {
    use super::*;
    use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
    use xcap::Monitor;

    fn monitors() -> Result<Vec<Monitor>, String> {
        Monitor::all().map_err(|e| e.to_string())
    }

    fn mon_name(m: &Monitor, i: usize) -> String {
        m.name().unwrap_or_else(|_| format!("display-{i}"))
    }

    fn pick(display: Option<i32>) -> Result<(Monitor, usize, String), String> {
        let all = monitors()?;
        if all.is_empty() {
            return Err("no displays (need a graphical session)".into());
        }
        if let Some(idx) = display {
            let i = idx as usize;
            let m = all
                .into_iter()
                .nth(i)
                .ok_or_else(|| format!("display {idx} is out of range"))?;
            let label = mon_name(&m, i);
            return Ok((m, i, label));
        }
        let primary = all.iter().position(|m| m.is_primary().unwrap_or(false));
        let i = primary.unwrap_or(0);
        let m = all.into_iter().nth(i).expect("non-empty");
        let label = mon_name(&m, i);
        Ok((m, i, label))
    }

    pub(super) fn list_displays_text() -> Result<String, String> {
        let all = monitors()?;
        if all.is_empty() {
            return Err("no displays (need a graphical session)".into());
        }
        let mut lines = Vec::new();
        for (i, m) in all.iter().enumerate() {
            let name = mon_name(m, i);
            let w = m.width().unwrap_or(0);
            let h = m.height().unwrap_or(0);
            let x = m.x().unwrap_or(0);
            let y = m.y().unwrap_or(0);
            let scale = m.scale_factor().unwrap_or(1.0);
            let primary = if m.is_primary().unwrap_or(false) {
                ", primary"
            } else {
                ""
            };
            lines.push(format!(
                "{i}: {name} {w}x{h} at {x},{y} scale={scale:.2}{primary}"
            ));
        }
        lines.push(
            "Pass display as a 0-based index to screenshot. Click x,y in the returned image."
                .into(),
        );
        Ok(lines.join("\n"))
    }

    pub(super) fn capture_display(
        display: Option<i32>,
    ) -> Result<(RgbaImage, ShotMeta, String), String> {
        let (monitor, _i, label) = pick(display)?;
        let origin_x = monitor.x().unwrap_or(0);
        let origin_y = monitor.y().unwrap_or(0);
        let screen_w = monitor.width().unwrap_or(0);
        let screen_h = monitor.height().unwrap_or(0);
        let captured = monitor.capture_image().map_err(|e| e.to_string())?;
        let phys_w = captured.width();
        let phys_h = captured.height();
        let raw = captured.into_raw();
        let rgba = RgbaImage::from_raw(phys_w, phys_h, raw)
            .ok_or_else(|| "invalid screenshot buffer".to_string())?;
        let meta = ShotMeta {
            origin_x,
            origin_y,
            screen_w: if screen_w == 0 { phys_w } else { screen_w },
            screen_h: if screen_h == 0 { phys_h } else { screen_h },
            img_w: phys_w,
            img_h: phys_h,
        };
        Ok((rgba, meta, label))
    }

    fn enigo() -> Result<Enigo, String> {
        Enigo::new(&Settings::default()).map_err(|e| e.to_string())
    }

    pub(super) fn play_input(action: Action, args: &Value) -> Result<String, String> {
        match action {
            Action::Move => {
                let (x, y) = super::resolve_point(args)?;
                let mut e = enigo()?;
                e.move_mouse(x, y, Coordinate::Abs)
                    .map_err(|err| err.to_string())?;
                Ok(format!("moved to {x},{y}"))
            }
            Action::Click | Action::DoubleClick | Action::RightClick => {
                let (x, y) = super::resolve_point(args)?;
                let mut e = enigo()?;
                e.move_mouse(x, y, Coordinate::Abs)
                    .map_err(|err| err.to_string())?;
                std::thread::sleep(Duration::from_millis(20));
                let btn = if action == Action::RightClick {
                    Button::Right
                } else {
                    Button::Left
                };
                e.button(btn, Direction::Click)
                    .map_err(|err| err.to_string())?;
                if action == Action::DoubleClick {
                    std::thread::sleep(Duration::from_millis(50));
                    e.button(Button::Left, Direction::Click)
                        .map_err(|err| err.to_string())?;
                    Ok(format!("double-clicked {x},{y}"))
                } else if action == Action::RightClick {
                    Ok(format!("right-clicked {x},{y}"))
                } else {
                    Ok(format!("clicked {x},{y}"))
                }
            }
            Action::Drag => {
                let (x, y) = super::resolve_point(args)?;
                let x2 = super::arg_f64(args, "x2")
                    .ok_or_else(|| "Error: drag needs numeric x2.".to_string())?;
                let y2 = super::arg_f64(args, "y2")
                    .ok_or_else(|| "Error: drag needs numeric y2.".to_string())?;
                let (x2, y2) = if let Some(meta) = super::last_shot() {
                    super::image_to_screen(x2, y2, &meta)
                } else {
                    (x2.round() as i32, y2.round() as i32)
                };
                let mut e = enigo()?;
                e.move_mouse(x, y, Coordinate::Abs)
                    .map_err(|err| err.to_string())?;
                std::thread::sleep(Duration::from_millis(20));
                e.button(Button::Left, Direction::Press)
                    .map_err(|err| err.to_string())?;
                std::thread::sleep(Duration::from_millis(30));
                e.move_mouse(x2, y2, Coordinate::Abs)
                    .map_err(|err| err.to_string())?;
                std::thread::sleep(Duration::from_millis(30));
                e.button(Button::Left, Direction::Release)
                    .map_err(|err| err.to_string())?;
                Ok(format!("dragged {x},{y} → {x2},{y2}"))
            }
            Action::Scroll => {
                let dy = super::arg_f64(args, "scroll_y")
                    .or_else(|| super::arg_f64(args, "delta"))
                    .ok_or_else(|| {
                        "Error: scroll needs scroll_y (positive is down).".to_string()
                    })?;
                let mut e = enigo()?;
                if super::arg_f64(args, "x").is_some() && super::arg_f64(args, "y").is_some() {
                    let (x, y) = super::resolve_point(args)?;
                    e.move_mouse(x, y, Coordinate::Abs)
                        .map_err(|err| err.to_string())?;
                    std::thread::sleep(Duration::from_millis(20));
                }
                let len = dy.round() as i32;
                if len == 0 {
                    return Ok("scroll 0 (no-op)".into());
                }
                e.scroll(len, Axis::Vertical)
                    .map_err(|err| err.to_string())?;
                Ok(format!("scrolled y={len}"))
            }
            Action::Type => {
                let text = super::arg_str(args, "text")
                    .or_else(|| super::arg_str(args, "string"))
                    .ok_or_else(|| "Error: type needs text.".to_string())?;
                if text.chars().count() > MAX_TYPE_CHARS {
                    return Err(format!(
                        "Error: type text exceeds {MAX_TYPE_CHARS} characters."
                    ));
                }
                let mut e = enigo()?;
                e.text(text).map_err(|err| err.to_string())?;
                Ok(format!("typed {} chars", text.chars().count()))
            }
            Action::Key => {
                let spec = super::arg_str(args, "keys")
                    .or_else(|| super::arg_str(args, "key"))
                    .or_else(|| super::arg_str(args, "text"))
                    .ok_or_else(|| {
                        "Error: key needs keys (e.g. enter, cmd+c, mod+v).".to_string()
                    })?;
                let mut e = enigo()?;
                play_keys(&mut e, spec)?;
                Ok(format!("pressed {spec}"))
            }
            Action::Screenshot | Action::ListDisplays | Action::Wait => {
                unreachable!("input path")
            }
        }
    }

    fn play_keys(e: &mut Enigo, spec: &str) -> Result<(), String> {
        let parts: Vec<&str> = spec
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err("Error: empty keys.".into());
        }
        let (mods, last) = parts.split_at(parts.len() - 1);
        let key = parse_key(last[0])?;
        let held: Vec<Key> = mods
            .iter()
            .map(|m| parse_mod(m))
            .collect::<Result<_, _>>()?;
        for m in &held {
            e.key(*m, Direction::Press).map_err(|err| err.to_string())?;
        }
        e.key(key, Direction::Click)
            .map_err(|err| err.to_string())?;
        for m in held.iter().rev() {
            e.key(*m, Direction::Release)
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    fn parse_mod(raw: &str) -> Result<Key, String> {
        parse_named_key(raw).ok_or_else(|| format!("Error: unknown modifier `{raw}`."))
    }

    fn parse_key(raw: &str) -> Result<Key, String> {
        if let Some(k) = parse_named_key(raw) {
            return Ok(k);
        }
        let t = raw.trim();
        let mut chars = t.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Ok(Key::Unicode(c));
        }
        Err(format!(
            "Error: unknown key `{raw}`. Use enter, tab, esc, cmd+c, ctrl+v, mod+c, or a single character."
        ))
    }

    fn parse_named_key(raw: &str) -> Option<Key> {
        let n = raw.trim().to_ascii_lowercase();
        Some(match n.as_str() {
            "cmd" | "command" | "super" | "win" | "meta" | "windows" => Key::Meta,
            "mod" => platform_mod(),
            "ctrl" | "control" | "ctl" => Key::Control,
            "alt" | "option" | "opt" => Key::Alt,
            "shift" => Key::Shift,
            "enter" | "return" => Key::Return,
            "esc" | "escape" => Key::Escape,
            "tab" => Key::Tab,
            "space" | "spacebar" => Key::Space,
            "backspace" | "bksp" => Key::Backspace,
            "delete" | "del" => Key::Delete,
            "up" | "uparrow" => Key::UpArrow,
            "down" | "downarrow" => Key::DownArrow,
            "left" | "leftarrow" => Key::LeftArrow,
            "right" | "rightarrow" => Key::RightArrow,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" | "pgup" => Key::PageUp,
            "pagedown" | "pgdn" => Key::PageDown,
            "caps" | "capslock" => Key::CapsLock,
            "f1" => Key::F1,
            "f2" => Key::F2,
            "f3" => Key::F3,
            "f4" => Key::F4,
            "f5" => Key::F5,
            "f6" => Key::F6,
            "f7" => Key::F7,
            "f8" => Key::F8,
            "f9" => Key::F9,
            "f10" => Key::F10,
            "f11" => Key::F11,
            "f12" => Key::F12,
            _ => return None,
        })
    }

    fn platform_mod() -> Key {
        #[cfg(target_os = "macos")]
        {
            Key::Meta
        }
        #[cfg(not(target_os = "macos"))]
        {
            Key::Control
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
use sys::{capture_display, list_displays_text, play_input};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(args: Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "ComputerUse".into(),
            arguments: args,
        }
    }

    #[test]
    fn action_aliases() {
        assert_eq!(
            parse_action_name(&json!({"action": "double_click"})),
            Some(Action::DoubleClick)
        );
        assert_eq!(
            parse_action_name(&json!({"action": "list-displays"})),
            Some(Action::ListDisplays)
        );
        assert_eq!(
            parse_action_name(&json!({"action": "hotkey"})),
            Some(Action::Key)
        );
        assert!(parse_action_name(&json!({"action": "explode"})).is_none());
    }

    #[test]
    fn observe_actions_skip_permit() {
        assert!(is_observe(&json!({"action": "screenshot"})));
        assert!(is_observe(&json!({"action": "wait", "ms": 1})));
        assert!(is_observe(&json!({"action": "list_displays"})));
        assert!(!is_observe(&json!({"action": "click", "x": 1, "y": 1})));
        assert!(!is_observe(&json!({"action": "type", "text": "a"})));
    }

    #[test]
    fn image_space_maps_onto_display() {
        let meta = ShotMeta {
            origin_x: 100,
            origin_y: 50,
            screen_w: 1000,
            screen_h: 500,
            img_w: 500,
            img_h: 250,
        };
        assert_eq!(image_to_screen(0.0, 0.0, &meta), (100, 50));
        assert_eq!(image_to_screen(250.0, 125.0, &meta), (600, 300));
        assert_eq!(image_to_screen(500.0, 250.0, &meta), (1099, 549));
    }

    #[test]
    fn jpeg_fits_inline_cap() {
        let mut rgba = RgbaImage::new(64, 48);
        for p in rgba.pixels_mut() {
            *p = image::Rgba([220, 40, 40, 255]);
        }
        let (bytes, w, h) = encode_screenshot(rgba, 1280, MAX_INLINE_MEDIA_BYTES).unwrap();
        assert_eq!((w, h), (64, 48));
        assert!(!bytes.is_empty());
        assert!(bytes.len() <= MAX_INLINE_MEDIA_BYTES);
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn wait_is_sync_success() {
        let r = execute_sync(&call(json!({"action": "wait", "ms": 1})));
        assert_eq!(r.state, ToolState::Success);
        assert!(r.joined_text().contains("waited 1ms"));
    }

    #[tokio::test]
    async fn pre_cancelled_returns_without_work() {
        let cancel = crate::tool_calls::CancelFlag::new();
        cancel.cancel();
        let started = std::time::Instant::now();
        let r = computer_use(&call(json!({"action": "screenshot"})), cancel, "t").await;
        assert_eq!(r.state, ToolState::Interrupted);
        assert!(r.joined_text().contains("aborted"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_returns_when_cancelled() {
        let cancel = crate::tool_calls::CancelFlag::new();
        let task = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                computer_use(&call(json!({"action": "wait", "ms": 8000})), cancel, "t").await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        cancel.cancel();
        let r = tokio::time::timeout(std::time::Duration::from_millis(500), task)
            .await
            .expect("ComputerUse wait ignored cancel")
            .expect("join");
        assert_eq!(r.state, ToolState::Interrupted, "{}", r.joined_text());
        assert!(r.joined_text().contains("aborted"));
    }

    #[test]
    fn click_without_xy_errors() {
        let r = execute_sync(&call(json!({"action": "click"})));
        assert_eq!(r.state, ToolState::Error);
        assert!(r.joined_text().starts_with("Error:"));
    }

    #[test]
    fn unknown_action_errors() {
        let r = execute_sync(&call(json!({"action": "explode"})));
        assert_eq!(r.state, ToolState::Error);
        assert!(r.joined_text().contains("action:"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn unsupported_os_errors() {
        let r = execute_sync(&call(json!({"action": "screenshot"})));
        assert_eq!(r.state, ToolState::Error);
        assert!(r.joined_text().contains("Windows"));
    }

    #[test]
    fn last_shot_is_keyed_by_session() {
        set_shot_session("cu-a");
        remember(ShotMeta {
            origin_x: 1,
            origin_y: 2,
            screen_w: 3,
            screen_h: 4,
            img_w: 5,
            img_h: 6,
        });
        set_shot_session("cu-b");
        remember(ShotMeta {
            origin_x: 10,
            origin_y: 20,
            screen_w: 30,
            screen_h: 40,
            img_w: 50,
            img_h: 60,
        });
        set_shot_session("cu-a");
        assert_eq!(last_shot().unwrap().origin_x, 1);
        set_shot_session("cu-b");
        assert_eq!(last_shot().unwrap().origin_x, 10);
    }
}
