//! Child-process flags for GUI/Electron hosts.
//!
//! On Windows a console-subsystem helper (git, bash, python, MCP) spawned from
//! a windowless sidecar can sit on a hidden console forever. `CREATE_NO_WINDOW`
//! is the same flag `media_exec` already uses for ffmpeg.

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn hide_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

pub fn hide_window_async(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

/// Keep reading so a child can exit; only the first `cap` bytes are kept.
/// After `cap`, discard up to 8MiB more then drop the pipe (`yes` / `cat /dev/zero`).
const DRAIN_DISCARD_MAX: usize = 8 * 1024 * 1024;

pub fn drain_capped<R: std::io::Read>(pipe: &mut R, buf: &mut Vec<u8>, cap: usize) {
    let mut tmp = [0u8; 8192];
    let mut discarded = 0usize;
    loop {
        match pipe.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&tmp[..n.min(room)]);
                    discarded = discarded.saturating_add(n.saturating_sub(room));
                } else {
                    discarded = discarded.saturating_add(n);
                }
                if discarded >= DRAIN_DISCARD_MAX {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

pub fn command_output_capped(
    cmd: &mut std::process::Command,
    cap: usize,
    timeout: std::time::Duration,
) -> std::io::Result<(bool, Vec<u8>, Vec<u8>)> {
    use std::process::Stdio;
    use std::time::Instant;
    hide_window(cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut o) = stdout {
            drain_capped(&mut o, &mut buf, cap);
        }
        buf
    });
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut o) = stderr {
            drain_capped(&mut o, &mut buf, cap);
        }
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_t.join();
                let _ = err_t.join();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "command timed out",
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = out_t.join();
                let _ = err_t.join();
                return Err(e);
            }
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn drain_capped_keeps_prefix_and_consumes_rest() {
        let mut src = Cursor::new(vec![1u8; 100]);
        let mut buf = Vec::new();
        drain_capped(&mut src, &mut buf, 10);
        assert_eq!(buf.len(), 10);
        assert_eq!(src.position(), 100);
    }

    #[test]
    fn drain_capped_stops_unbounded_reader() {
        struct Endless;
        impl std::io::Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(1);
                Ok(buf.len())
            }
        }
        let mut buf = Vec::new();
        let started = std::time::Instant::now();
        drain_capped(&mut Endless, &mut buf, 10);
        assert_eq!(buf.len(), 10);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "unbounded reader must not hang the cap drain"
        );
    }

    #[test]
    fn command_output_capped_keeps_prefix() {
        let mut cmd = std::process::Command::new("python3");
        cmd.args(["-c", "print('x'*100, end='')"]);
        let (ok, stdout, _) =
            command_output_capped(&mut cmd, 10, std::time::Duration::from_secs(3)).unwrap();
        assert!(ok);
        assert_eq!(stdout.len(), 10);
    }
}
