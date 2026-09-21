//! Capture panic payload + backtrace for turn recovery. JSONL gets a short
//! redacted line; the full backtrace goes to `~/.grok-hyper/desktop.log`.

use std::backtrace::Backtrace;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct RecordedPanic {
    pub message: String,
    pub location: Option<String>,
    pub backtrace: String,
}

static LAST: Mutex<Option<RecordedPanic>> = Mutex::new(None);
static HOOK: OnceLock<()> = OnceLock::new();

pub fn install_once() {
    HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            record(info);
            prev(info);
        }));
    });
}

fn record(info: &PanicHookInfo<'_>) {
    let message = panic_payload_text(info.payload()).unwrap_or_else(|| info.to_string());
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let backtrace = Backtrace::force_capture().to_string();
    if let Ok(mut g) = LAST.lock() {
        *g = Some(RecordedPanic {
            message,
            location,
            backtrace,
        });
    }
}

pub fn take() -> Option<RecordedPanic> {
    LAST.lock().ok().and_then(|mut g| g.take())
}

pub fn panic_payload_text(payload: &(dyn std::any::Any + Send)) -> Option<String> {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
}

pub fn format_user_message(
    payload: &(dyn std::any::Any + Send),
    recorded: Option<&RecordedPanic>,
) -> String {
    let raw = recorded
        .map(|r| r.message.as_str())
        .map(str::to_string)
        .or_else(|| panic_payload_text(payload))
        .unwrap_or_else(|| "unknown panic".into());
    crate::secrets::redact(&raw)
}

pub fn backtrace_hash(recorded: Option<&RecordedPanic>) -> String {
    let Some(r) = recorded else {
        return "none".into();
    };
    let mut h = Sha256::new();
    h.update(r.backtrace.as_bytes());
    let hex = format!("{:x}", h.finalize());
    hex.chars().take(16).collect()
}

pub fn summary_line(
    run_id: &str,
    turn_id: &str,
    step_id: u32,
    last_tool: Option<&str>,
    recorded: Option<&RecordedPanic>,
    msg: &str,
) -> String {
    let loc = recorded.and_then(|r| r.location.as_deref()).unwrap_or("-");
    format!(
        "run_id={run_id} turn_id={turn_id} step_id={step_id} last_tool={} loc={loc} msg={} bt={}",
        last_tool.unwrap_or("-"),
        crate::secrets::redact(msg),
        backtrace_hash(recorded)
    )
}

pub fn write_desktop_log(recorded: Option<&RecordedPanic>, summary: &str) {
    let Ok(home) = crate::config::Config::home_dir() else {
        eprintln!("hyper: turn panicked: {summary}");
        return;
    };
    let path = home.join("desktop.log");
    let mut body = String::new();
    body.push_str(summary);
    body.push('\n');
    if let Some(r) = recorded {
        if let Some(loc) = &r.location {
            body.push_str("location: ");
            body.push_str(loc);
            body.push('\n');
        }
        body.push_str(&crate::secrets::redact(&r.backtrace));
        body.push('\n');
    }
    if let Some(parent) = path.parent() {
        let _ = crate::fs_mode::ensure_private_dir(parent);
    }
    if crate::tools::is_special_file(&path) {
        eprintln!("hyper: turn panicked: {summary}");
        return;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&path) {
        Ok(mut f) => {
            let _ = f.write_all(body.as_bytes());
            crate::fs_mode::tighten_file(&path);
        }
        Err(_) => eprintln!("hyper: turn panicked: {summary}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_from_string_ref() {
        let s: &str = "boom";
        let boxed: Box<dyn std::any::Any + Send> = Box::new(s);
        assert_eq!(panic_payload_text(&*boxed).as_deref(), Some("boom"));
    }

    #[test]
    fn hash_is_stable() {
        let rec = RecordedPanic {
            message: "x".into(),
            location: None,
            backtrace: "stack".into(),
        };
        assert_eq!(backtrace_hash(Some(&rec)).len(), 16);
        assert_eq!(backtrace_hash(Some(&rec)), backtrace_hash(Some(&rec)));
    }
}
