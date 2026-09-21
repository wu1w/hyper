//! `run_code`: user-equivalent `python3 -I` subprocess (not a sandbox).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{arg_str, folded_response, BlobStore, ToolLimits, Workspace};
use crate::tool_calls::{CancelFlag, ToolCall, ToolResponse, ToolState};

const SDK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/tools/hyper_sdk.py"
));
const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// Same as Shell: after the live cap, discard a bounded extra then drop the
/// pipe so `print('\\0'*n)` / `os.write(1, zeros)` cannot hang the hop.
const DISCARD_MAX_BYTES: usize = 8 * 1024 * 1024;

pub async fn run_code(
    ws: &Workspace,
    call: &ToolCall,
    cancel: CancelFlag,
    limits: ToolLimits,
    inherit_env: bool,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(code) = arg_str(&call.arguments, "code") else {
        return ToolResponse::text(&call.id, "Error: No `code` provided.", ToolState::Error);
    };
    if let Some(rel) = super::path::first_special_quoted_path(ws.root(), &code)
        .or_else(|| super::path::first_special_unquoted_open(ws.root(), &code))
    {
        return ToolResponse::text(
            &call.id,
            format!("Error: {rel} is not a regular file."),
            ToolState::Error,
        );
    }
    if let Some(rel) = super::path::first_oversized_quoted_path(ws.root(), &code) {
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: {rel} is too large to slurp (max {} bytes).",
                super::path::MAX_TEXT_SLURP_BYTES
            ),
            ToolState::Error,
        );
    }

    let script = match write_script(ws.root(), &code) {
        Ok(p) => p,
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!(
                    "Error: failed to write run_code script: {}",
                    super::path::io_user_msg(&e)
                ),
                ToolState::Error,
            );
        }
    };

    let mut child = match spawn_python(&script.path, ws.root(), inherit_env) {
        Ok(c) => c,
        Err(e) => {
            script.cleanup();
            return ToolResponse::text(
                &call.id,
                format!(
                    "Error: failed to spawn python: {}",
                    super::path::io_user_msg(&e)
                ),
                ToolState::Error,
            );
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(async move {
        match stdout {
            Some(p) => read_capped(p).await,
            None => (String::new(), 0),
        }
    });
    let err_task = tokio::spawn(async move {
        match stderr {
            Some(p) => read_capped(p).await,
            None => (String::new(), 0),
        }
    });

    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;
            ToolResponse::text(
                &call.id,
                "Command failed with exit code -1.\n[stderr]\ncancelled",
                ToolState::Interrupted,
            )
        }
        status = child.wait() => {
            let (stdout, out_disc) = out_task.await.unwrap_or_default();
            let (stderr, err_disc) = err_task.await.unwrap_or_default();
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let mut text = format_result(code, &stdout, &stderr);
            let mut state = if code == 0 {
                ToolState::Success
            } else {
                ToolState::Error
            };
            if out_disc + err_disc > 0 {
                if !text.starts_with("Error:") {
                    text = format!(
                        "Error: command output truncated after {OUTPUT_MAX_BYTES} bytes (incomplete).\n{text}"
                    );
                }
                state = ToolState::Error;
            }
            folded_response(&call.id, text, state, limits, blobs)
        }
    };
    script.cleanup();
    response
}

struct TempScript {
    path: PathBuf,
}

impl TempScript {
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn write_script(root: &Path, user_code: &str) -> std::io::Result<TempScript> {
    let dir = root.join(".hyper-sdk");
    std::fs::create_dir_all(&dir)?;
    let id = uuid::Uuid::new_v4().simple();
    let path = dir.join(format!("run_{id}.py"));
    let root_json = serde_json::to_string(&root.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "\"\"".into());
    let body = format!("_HYPER_ROOT = {root_json}\n{SDK}\n\n{user_code}\n");
    super::write_if_regular(&path, body.as_bytes())?;
    Ok(TempScript { path })
}

fn spawn_python(
    script: &Path,
    cwd: &Path,
    inherit_env: bool,
) -> std::io::Result<tokio::process::Child> {
    let mut last_not_found = None;
    for bin in ["python3", "python"] {
        match spawn_python_bin(bin, script, cwd, inherit_env) {
            Ok(child) => return Ok(child),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                last_not_found = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_not_found.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "python3/python not found")
    }))
}

fn spawn_python_bin(
    bin: &str,
    script: &Path,
    cwd: &Path,
    inherit_env: bool,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = Command::new(bin);
    cmd.arg("-I")
        .arg("-u")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env(&mut cmd, inherit_env);
    crate::proc_spawn::hide_window_async(&mut cmd);
    cmd.spawn()
}

fn apply_env(cmd: &mut Command, inherit_env: bool) {
    if inherit_env {
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        return;
    }
    cmd.env_clear();
    for key in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "LANG",
        "SystemRoot",
        "PATHEXT",
        "COMSPEC",
    ] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
}

fn format_result(code: i32, stdout: &str, stderr: &str) -> String {
    if code == 0 {
        let mut text = if stdout.is_empty() {
            "Command executed successfully (no output).".to_string()
        } else {
            stdout.to_string()
        };
        if !stderr.is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(stderr);
        }
        text
    } else {
        let mut parts = vec![format!("Command failed with exit code {code}.")];
        if !stdout.is_empty() {
            parts.push(format!("\n[stdout]\n{stdout}"));
        }
        if !stderr.is_empty() {
            parts.push(format!("\n[stderr]\n{stderr}"));
        }
        parts.concat()
    }
}

async fn read_capped<R: AsyncRead + Unpin>(mut pipe: R) -> (String, usize) {
    let mut buf = Vec::new();
    let mut discarded = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let room = OUTPUT_MAX_BYTES.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                    discarded = discarded.saturating_add(n.saturating_sub(room));
                } else {
                    discarded = discarded.saturating_add(n);
                }
                if discarded >= DISCARD_MAX_BYTES {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if buf.contains(&0) {
        let mut text = format!("[binary output omitted: {} bytes contain NUL]", buf.len());
        if discarded > 0 {
            text.push_str(&format!(
                "\n… truncated after {OUTPUT_MAX_BYTES} bytes ({} more discarded).",
                discarded
            ));
        }
        (text, discarded)
    } else {
        let mut text = String::from_utf8_lossy(&buf).into_owned();
        if discarded > 0 {
            text.push_str(&format!(
                "\n… truncated after {OUTPUT_MAX_BYTES} bytes ({} more discarded).",
                discarded
            ));
        }
        (text, discarded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::ToolCall;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch() -> (Workspace, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("hyper-runcode-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Workspace::open(&dir, true).unwrap();
        (w, dir)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "run_code".into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn sdk_write_and_read() {
        let (ws, dir) = scratch();
        let out = run_code(
            &ws,
            &call(json!({
                "code": "write('n.txt', 'hello from sdk')\nprint(read('n.txt'), end='')"
            })),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("hello from sdk"),
            "{}",
            out.joined_text()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("n.txt")).unwrap(),
            "hello from sdk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_fifo_in_code_is_error_not_hang() {
        let (ws, dir) = scratch();
        let fifo = dir.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let out = run_code(
            &ws,
            &call(json!({"code": "print(open('pipe.txt').read())"})),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "run_code FIFO open must not block: {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("regular"),
            "{}",
            out.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_var_fifo_in_code_is_error_not_hang() {
        let (ws, dir) = scratch();
        let fifo = dir.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let out = run_code(
            &ws,
            &call(json!({"code": "p=os.listdir('.')[0]\nprint(open(p).read())"})),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "run_code open(p) FIFO must not block: {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("regular"),
            "{}",
            out.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn system_listdir_fifo_in_code_is_error_not_hang() {
        let (ws, dir) = scratch();
        let fifo = dir.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let out = run_code(
            &ws,
            &call(json!({"code": "import os\nos.system('cat '+os.listdir('.')[0])"})),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "run_code os.system listdir FIFO must not block: {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("regular"),
            "{}",
            out.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn open_oversized_in_code_is_error_not_slurp() {
        let (ws, dir) = scratch();
        std::fs::write(
            dir.join("huge.txt"),
            vec![b'x'; (super::super::path::MAX_TEXT_SLURP_BYTES as usize) + 1],
        )
        .unwrap();
        let started = std::time::Instant::now();
        let out = run_code(
            &ws,
            &call(json!({"code": "print(open('huge.txt').read())"})),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "run_code open oversized must not slurp: {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("too large") || out.joined_text().contains("slurp"),
            "{}",
            out.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn confined_write_rejects_escape() {
        let (ws, dir) = scratch();
        let name = format!("hyper-escape-{}.txt", uuid::Uuid::new_v4().simple());
        let outside = dir.parent().unwrap().join(&name);
        let code = format!(
            "try:\n    write('../{name}', 'no')\n    print('ESCAPED')\nexcept Exception:\n    print('DENIED')\n"
        );
        let out = run_code(
            &ws,
            &call(json!({ "code": code })),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("DENIED"),
            "{}",
            out.joined_text()
        );
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_write_rejects_symlink_out() {
        use std::os::unix::fs::symlink;

        let (ws, dir) = scratch();
        let outside =
            std::env::temp_dir().join(format!("hyper-runcode-out-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dir.join("hole")).unwrap();
        let out = run_code(
            &ws,
            &call(json!({
                "code": "try:\n    write('hole/new.txt', 'no')\n    print('ESCAPED')\nexcept Exception:\n    print('DENIED')\n"
            })),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("DENIED"),
            "{}",
            out.joined_text()
        );
        assert!(!outside.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[tokio::test]
    async fn sdk_edit_requires_unique_old() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.txt"), "x\ny\nx\n").unwrap();
        let out = run_code(
            &ws,
            &call(json!({
                "code": "try:\n    edit('a.txt', 'x', 'X')\n    print('MULTI')\nexcept Exception as e:\n    print('UNIQUE' if 'unique' in str(e) else e)\n"
            })),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("UNIQUE"),
            "{}",
            out.joined_text()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "x\ny\nx\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn sdk_edit_preserves_crlf_for_lf_request() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.txt"), b"def f():\r\n    return 1\r\nkeep\r\n").unwrap();
        let out = run_code(
            &ws,
            &call(json!({
                "code": "print(edit('a.txt', 'def f():\\n    return 1', 'def f():\\n    return 2'))"
            })),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("preserved file line endings"),
            "{}",
            out.joined_text()
        );
        assert_eq!(
            std::fs::read(dir.join("a.txt")).unwrap(),
            b"def f():\r\n    return 2\r\nkeep\r\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_capped_drains_past_cap_to_eof() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(8192);
        let reader_task = tokio::spawn(async move { read_capped(reader).await });
        let writer_task = tokio::spawn(async move {
            let first = vec![b'a'; OUTPUT_MAX_BYTES + 256 * 1024];
            writer.write_all(&first).await?;
            writer.write_all(b"TAIL").await?;
            Ok::<_, std::io::Error>(())
        });
        let (out, discarded) = tokio::time::timeout(std::time::Duration::from_secs(5), reader_task)
            .await
            .expect("read_capped hung")
            .expect("join");
        writer_task
            .await
            .expect("writer join")
            .expect("follow-on write must complete (pipe drained to EOF)");
        assert!(out.starts_with("aaaa"));
        assert!(!out.contains("TAIL"));
        assert!(out.contains("truncated after"), "{out}");
        assert!(discarded > 0, "{discarded}");
    }

    #[tokio::test]
    async fn read_capped_stops_unbounded_writer() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(8192);
        let reader_task = tokio::spawn(async move { read_capped(reader).await });
        let writer_task = tokio::spawn(async move {
            let chunk = vec![b'y'; 8192];
            loop {
                if writer.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let (out, discarded) = tokio::time::timeout(std::time::Duration::from_secs(3), reader_task)
            .await
            .expect("unbounded producer must not hang the cap reader")
            .expect("join");
        drop(writer_task);
        assert!(out.starts_with("yyyy"));
        assert!(out.contains("truncated after"), "{out}");
        assert!(discarded > 0, "{discarded}");
    }

    #[tokio::test]
    async fn run_code_nul_stdout_is_omitted() {
        let (ws, dir) = scratch();
        let out = run_code(
            &ws,
            &call(json!({"code": "import sys; sys.stdout.buffer.write(b'\\x00hello')"})),
            CancelFlag::new(),
            ToolLimits::default(),
            true,
            None,
        )
        .await;
        let t = out.joined_text();
        assert!(!t.contains('\0'), "{t:?}");
        assert!(t.contains("binary") || t.contains("NUL"), "{t}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn run_code_large_stdout_returns_prefix_without_hang() {
        let (ws, dir) = scratch();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            run_code(
                &ws,
                &call(json!({
                    "code": "print('a'*2_000_000, end='')"
                })),
                CancelFlag::new(),
                ToolLimits::default(),
                true,
                None,
            ),
        )
        .await
        .expect("run_code hung on large stdout");
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        let live = out.joined_text();
        assert!(live.starts_with("Error:"), "{live}");
        assert!(live.contains("aaa"), "{live}");
        assert!(live.contains("truncated after"), "{live}");
        assert!(!live.contains("Command failed"), "{live}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_bash_times_out() {
        let (ws, dir) = scratch();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            run_code(
                &ws,
                &call(json!({
                    "code": "import os\nos.environ['HYPER_BASH_TIMEOUT'] = '1'\ntry:\n    bash('sleep 20')\n    print('NO_TIMEOUT')\nexcept Exception as e:\n    print('TIMEOUT' if 'timed out' in str(e).lower() else e)\n"
                })),
                CancelFlag::new(),
                ToolLimits::default(),
                true,
                None,
            ),
        )
        .await
        .expect("sdk bash timeout hung");
        let live = out.joined_text();
        assert!(live.contains("TIMEOUT"), "{live}");
        assert!(!live.contains("NO_TIMEOUT"), "{live}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
