//! Start/stop the local OnlyOffice Document Server via Docker.
//!
//! `hyper web` (and the desktop shell) call [`ensure_office`] in the background so
//! the user never runs `hyper office up`. Docker itself is still required — the
//! Document Server's converter is a Linux binary. We look up `docker` / `colima`
//! at well-known paths because Finder-launched apps have a tiny PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hyper_loop::config::{user_home, OfficeConfig};

use crate::office::docs_ready;

pub const CONTAINER: &str = "hyper-onlyoffice";
pub const IMAGE: &str = "onlyoffice/documentserver";

const WAKE_SECS: u64 = 90;
const READY_SECS: u64 = 180;

#[derive(Clone)]
pub struct OfficeBoot {
    starting: Arc<AtomicBool>,
    hint: Arc<Mutex<String>>,
}

impl Default for OfficeBoot {
    fn default() -> Self {
        Self {
            starting: Arc::new(AtomicBool::new(false)),
            hint: Arc::new(Mutex::new(String::new())),
        }
    }
}

impl OfficeBoot {
    pub fn set(&self, starting: bool, hint: impl Into<String>) {
        self.starting.store(starting, Ordering::Relaxed);
        if let Ok(mut g) = self.hint.lock() {
            *g = hint.into();
        }
    }

    pub fn snapshot(&self) -> (bool, String) {
        let hint = self.hint.lock().map(|g| g.clone()).unwrap_or_default();
        (self.starting.load(Ordering::Relaxed), hint)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EnsureOpts {
    /// `docker pull` when the image is missing. First run is several GB.
    pub pull: bool,
    /// Poll `/healthcheck` after the container is up.
    pub wait_ready: bool,
    /// Start Colima / Docker.app if the engine is asleep.
    pub wake_daemon: bool,
}

impl EnsureOpts {
    pub fn cli_up() -> Self {
        Self {
            pull: true,
            wait_ready: true,
            wake_daemon: true,
        }
    }

    pub fn web_auto() -> Self {
        Self {
            pull: office_env_on("HYPER_OFFICE_PULL", true),
            wait_ready: true,
            wake_daemon: true,
        }
    }
}

#[derive(Debug)]
pub struct EnsureReport {
    pub ready: bool,
}

#[derive(Debug)]
pub enum EnsureError {
    NoDocker,
    Other(anyhow::Error),
}

impl EnsureError {
    pub fn user_hint(&self) -> String {
        match self {
            Self::NoDocker => "完整编辑需要本机 Docker，当前使用内置预览。".into(),
            Self::Other(_) => "文档服务暂时不可用，当前使用内置预览。".into(),
        }
    }
}

impl std::fmt::Display for EnsureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDocker => write!(f, "需要 Docker：https://docs.docker.com/get-docker/"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for EnsureError {}

pub fn office_auto_enabled() -> bool {
    office_env_on("HYPER_OFFICE_AUTO", true)
}

fn office_env_on(name: &str, default: bool) -> bool {
    parse_env_flag(std::env::var(name).ok().as_deref(), default)
}

fn parse_env_flag(raw: Option<&str>, default: bool) -> bool {
    match raw {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => default,
    }
}

pub async fn ensure_office(
    office: &OfficeConfig,
    opts: EnsureOpts,
) -> std::result::Result<EnsureReport, EnsureError> {
    let origin = office.docs_origin();
    if docs_ready(&origin).await {
        return Ok(EnsureReport { ready: true });
    }

    let office = office.clone();
    tokio::task::spawn_blocking(move || start_container_sync(&office, opts))
        .await
        .map_err(|e| EnsureError::Other(anyhow!("join: {e}")))??;

    if !opts.wait_ready {
        return Ok(EnsureReport {
            ready: docs_ready(&origin).await,
        });
    }
    Ok(EnsureReport {
        ready: wait_docs_ready(&origin, Duration::from_secs(READY_SECS)).await,
    })
}

pub fn stop_office() -> Result<bool> {
    docker_or_err()?;
    let st = docker_cmd()?
        .args(["rm", "-f", CONTAINER])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("docker rm")?;
    Ok(st.success())
}

pub fn docker_cli_ok() -> bool {
    find_docker().is_some()
}

/// `None` = no container. `Some(running)`.
pub fn container_running() -> Option<bool> {
    let out = docker_cmd()
        .ok()?
        .args(["inspect", "-f", "{{.State.Running}}", CONTAINER])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "true")
}

async fn wait_docs_ready(url: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if docs_ready(url).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

fn start_container_sync(
    office: &OfficeConfig,
    opts: EnsureOpts,
) -> std::result::Result<(), EnsureError> {
    if opts.wake_daemon {
        wake_daemon().map_err(|e| match e {
            EnsureError::NoDocker => EnsureError::NoDocker,
            other => other,
        })?;
    } else {
        docker_or_err().map_err(|_| EnsureError::NoDocker)?;
    }

    if container_running() == Some(true) {
        return Ok(());
    }
    if container_running() == Some(false) {
        let st = docker_cmd()
            .map_err(EnsureError::Other)?
            .args(["start", CONTAINER])
            .status()
            .context("docker start")
            .map_err(EnsureError::Other)?;
        if !st.success() {
            return Err(EnsureError::Other(anyhow!(
                "docker start {CONTAINER} failed"
            )));
        }
        eprintln!("office: started {CONTAINER}");
        return Ok(());
    }

    if !image_present() {
        if !opts.pull {
            return Err(EnsureError::Other(anyhow!(
                "image {IMAGE} is not present (set HYPER_OFFICE_PULL=1 or run hyper office up)"
            )));
        }
        eprintln!("office: pulling {IMAGE} (first run is large) …");
        let st = docker_cmd()
            .map_err(EnsureError::Other)?
            .args(["pull", IMAGE])
            .status()
            .context("docker pull")
            .map_err(EnsureError::Other)?;
        if !st.success() {
            return Err(EnsureError::Other(anyhow!("docker pull {IMAGE} failed")));
        }
    }

    let jwt = format!("JWT_SECRET={}", office.jwt_secret.trim());
    eprintln!("office: docker run {CONTAINER}");
    let st = docker_cmd()
        .map_err(EnsureError::Other)?
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            "127.0.0.1:8080:80",
            "-e",
            &jwt,
            "-e",
            "JWT_ENABLED=true",
            "-e",
            "ALLOW_PRIVATE_IP_ADDRESS=true",
            "-e",
            "ALLOW_META_IP_ADDRESS=true",
            "--add-host=host.docker.internal:host-gateway",
            "--restart",
            "unless-stopped",
            IMAGE,
        ])
        .status()
        .context("docker run")
        .map_err(EnsureError::Other)?;
    if !st.success() {
        return Err(EnsureError::Other(anyhow!("docker run {IMAGE} failed")));
    }
    Ok(())
}

fn wake_daemon() -> std::result::Result<(), EnsureError> {
    if find_docker().is_none() {
        return Err(EnsureError::NoDocker);
    }
    if docker_info_ok() {
        return Ok(());
    }
    if let Some(colima) = find_in_search_dirs("colima") {
        eprintln!("office: starting colima …");
        let _ = Command::new(colima)
            .arg("start")
            .env("PATH", docker_path())
            .status();
        if wait_info(Duration::from_secs(WAKE_SECS)) {
            return Ok(());
        }
    }
    if cfg!(target_os = "macos") && Path::new("/Applications/Docker.app").is_dir() {
        eprintln!("office: opening Docker.app …");
        let _ = Command::new("/usr/bin/open")
            .args(["-a", "Docker"])
            .status();
        if wait_info(Duration::from_secs(WAKE_SECS)) {
            return Ok(());
        }
    }
    if docker_info_ok() {
        return Ok(());
    }
    Err(EnsureError::Other(anyhow!("docker engine is not running")))
}

fn wait_info(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if docker_info_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

fn docker_info_ok() -> bool {
    docker_cmd()
        .and_then(|mut c| {
            c.args(["info"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("docker info")
        })
        .map(|s| s.success())
        .unwrap_or(false)
}

fn docker_or_err() -> Result<()> {
    find_docker()
        .map(|_| ())
        .ok_or_else(|| anyhow!("需要 Docker：https://docs.docker.com/get-docker/"))
}

fn image_present() -> bool {
    docker_cmd()
        .and_then(|mut c| {
            c.args(["image", "inspect", IMAGE])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("docker image inspect")
        })
        .map(|s| s.success())
        .unwrap_or(false)
}

fn docker_cmd() -> Result<Command> {
    let bin = find_docker().ok_or_else(|| anyhow!("docker not found"))?;
    let mut c = Command::new(bin);
    c.env("PATH", docker_path());
    hyper_loop::hide_window(&mut c);
    Ok(c)
}

fn find_docker() -> Option<PathBuf> {
    find_in_search_dirs(if cfg!(windows) {
        "docker.exe"
    } else {
        "docker"
    })
}

fn find_in_search_dirs(name: &str) -> Option<PathBuf> {
    for dir in docker_search_dirs() {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn docker_path() -> std::ffi::OsString {
    let dirs = docker_search_dirs();
    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in &dirs {
        if dir.is_dir() && seen.insert(dir.clone()) {
            ordered.push(dir.clone());
        }
    }
    if let Some(cur) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&cur) {
            if dir.is_dir() && seen.insert(dir.clone()) {
                ordered.push(dir);
            }
        }
    }
    std::env::join_paths(&ordered).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

fn docker_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = user_home() {
        dirs.push(home.join(".docker/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".colima/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from(
        "/Applications/Docker.app/Contents/Resources/bin",
    ));
    dirs.push(PathBuf::from("/usr/bin"));
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        dirs.push(PathBuf::from(pf).join("Docker/Docker/resources/bin"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_dirs_cover_gui_launch_paths() {
        let dirs = docker_search_dirs();
        assert!(dirs
            .iter()
            .any(|p| p.ends_with("homebrew/bin") || p == Path::new("/opt/homebrew/bin")));
        assert!(dirs
            .iter()
            .any(|p| p.to_string_lossy().contains("Docker.app")));
    }

    #[test]
    fn auto_env_defaults_on() {
        assert!(parse_env_flag(None, true));
        assert!(!parse_env_flag(Some("0"), true));
        assert!(!parse_env_flag(Some("off"), true));
        assert!(parse_env_flag(Some("1"), true));
    }

    #[test]
    fn no_docker_hint_is_user_facing() {
        let h = EnsureError::NoDocker.user_hint();
        assert!(!h.contains("hyper office"));
        assert!(h.contains("Docker"));
    }
}
