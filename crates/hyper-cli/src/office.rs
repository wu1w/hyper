//! `hyper office` — start/stop/status for the local OnlyOffice Document Server.
//! Desktop / `hyper web` call the same ensure path automatically.

use std::process::ExitCode;

use anyhow::{Context, Result};
use hyper_loop::config::Config;
use hyper_web::{
    container_running, docker_cli_ok, docs_ready, ensure_office, persist_office_secret,
    stop_office, EnsureOpts, CONTAINER,
};

#[derive(Debug, clap::Subcommand)]
pub enum OfficeAction {
    /// Pull and start the Document Server container.
    Up,
    /// Stop and remove the container.
    Down,
    /// Docker + healthcheck.
    Status,
}

pub async fn run(action: OfficeAction) -> Result<ExitCode> {
    let (_cfg, cfg_path) = Config::load_or_init().context("load config")?;
    let office = persist_office_secret(&cfg_path).context("office jwt_secret")?;
    match action {
        OfficeAction::Up => match ensure_office(&office, EnsureOpts::cli_up()).await {
            Ok(r) => {
                eprintln!("docs_url      {}", office.docs_origin());
                eprintln!(
                    "healthcheck   {}",
                    if r.ready { "ready" } else { "not ready" }
                );
                if !r.ready {
                    eprintln!("Document Server is booting; retry in a minute.");
                }
                Ok(if r.ready {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(2)
                })
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(ExitCode::from(1))
            }
        },
        OfficeAction::Down => {
            if stop_office().context("office down")? {
                eprintln!("stopped {CONTAINER}");
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("{CONTAINER} was not running");
                Ok(ExitCode::from(1))
            }
        }
        OfficeAction::Status => status(&office).await,
    }
}

async fn status(office: &hyper_loop::config::OfficeConfig) -> Result<ExitCode> {
    let docker_ok = docker_cli_ok();
    let running = docker_ok && container_running() == Some(true);
    let ready = docs_ready(&office.docs_origin()).await;
    eprintln!("docker        {}", if docker_ok { "ok" } else { "missing" });
    eprintln!(
        "container     {}",
        if running { CONTAINER } else { "not running" }
    );
    eprintln!("docs_url      {}", office.docs_origin());
    eprintln!(
        "healthcheck   {}",
        if ready { "ready" } else { "not ready" }
    );
    if !docker_ok {
        eprintln!("install Docker: https://docs.docker.com/get-docker/");
    } else if !running && !ready {
        eprintln!("start with: hyper office up");
    } else if !ready {
        eprintln!("Document Server is booting; retry in a minute.");
    }
    Ok(if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}
