//! Install Hyper as a VS Code / Cursor extension. Loop stays `hyper --sidecar`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{bail, Context, Result};
use hyper_loop::config::Config;
use serde_json::Value;

const PLUGIN: &str = "vscode-hyper";

pub struct VscodeInstallOpts {
    pub dry_run: bool,
}

pub fn run(opts: VscodeInstallOpts) -> Result<ExitCode> {
    let src = find_plugin_dir().context("find plugins/vscode-hyper")?;
    let home = Config::home_dir()?;
    let build = home.join(PLUGIN);
    let hyper_bin = resolve_hyper_bin();
    let ext_dir = extension_folder_name(&src)?;
    let editors = editor_extension_dirs();

    println!("plugin source  {}", src.display());
    println!("build copy     {}", build.display());
    println!("hyper binary   {}", hyper_bin.display());
    for dir in &editors {
        println!("editor ext     {}", dir.join(&ext_dir).display());
    }

    if opts.dry_run {
        println!("dry-run: no copies, no tsc");
        return Ok(ExitCode::SUCCESS);
    }

    if src.canonicalize().ok() != build.canonicalize().ok() {
        copy_dir(&src, &build).context("copy plugin into ~/.grok-hyper")?;
    }
    build_plugin(&build).context("compile extension (tsc)")?;

    for dir in editors {
        let dest = dir.join(&ext_dir);
        fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        install_unpacked(&build, &dest, &hyper_bin)
            .with_context(|| format!("install {}", dest.display()))?;
        println!("installed      {}", dest.display());
    }

    println!();
    println!("ok. Reload Window in VS Code / Cursor.");
    println!("  活动栏 Hyper 图标 → Chat");
    println!("models/keys still live in ~/.grok-hyper/config.toml");
    println!("set hyper.command if the editor cannot see this binary:");
    println!("  {}", hyper_bin.display());
    Ok(ExitCode::SUCCESS)
}

fn looks_like_plugin(dir: &Path) -> bool {
    dir.join("package.json").is_file()
        && dir.join("src/extension.ts").is_file()
        && dir.join("media/chat.js").is_file()
}

fn find_plugin_dir() -> Result<PathBuf> {
    if let Ok(explicit) = env::var("HYPER_VSCODE_PLUGIN") {
        let p = PathBuf::from(explicit);
        if looks_like_plugin(&p) {
            return Ok(p);
        }
        bail!("HYPER_VSCODE_PLUGIN is not a plugin dir: {}", p.display());
    }
    let mut candidates = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        candidates.extend(walk_parents(&cwd));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.extend(walk_parents(dir));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/vscode-hyper"));
    if let Ok(home) = Config::home_dir() {
        candidates.push(home.join(PLUGIN));
    }
    for root in candidates {
        let plugin = if looks_like_plugin(&root) {
            root
        } else {
            root.join("plugins").join(PLUGIN)
        };
        if looks_like_plugin(&plugin) {
            return Ok(plugin.canonicalize().unwrap_or(plugin));
        }
    }
    bail!("cannot find plugins/vscode-hyper (run from the grok-hyper checkout, or set HYPER_VSCODE_PLUGIN)");
}

fn walk_parents(start: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(p) = cur {
        out.push(p.to_path_buf());
        cur = p.parent();
    }
    out
}

fn user_home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_hyper_bin() -> PathBuf {
    if let Ok(explicit) = env::var("HYPER_BIN") {
        let p = PathBuf::from(explicit);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Ok(exe) = env::current_exe() {
        return exe;
    }
    PathBuf::from("hyper")
}

fn extension_folder_name(plugin: &Path) -> Result<String> {
    let raw = fs::read_to_string(plugin.join("package.json"))
        .with_context(|| format!("read {}", plugin.join("package.json").display()))?;
    let pkg: Value = serde_json::from_str(&raw).context("parse package.json")?;
    let publisher = pkg
        .get("publisher")
        .and_then(|v| v.as_str())
        .unwrap_or("wu1w");
    let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("hyper");
    let version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.1.0");
    Ok(format!("{publisher}.{name}-{version}"))
}

fn editor_extension_dirs() -> Vec<PathBuf> {
    let home = user_home();
    let mut out = vec![home.join(".vscode").join("extensions")];
    if home.join(".cursor").is_dir() {
        out.push(home.join(".cursor").join("extensions"));
    }
    if home.join(".vscode-oss").is_dir() {
        out.push(home.join(".vscode-oss").join("extensions"));
    }
    out
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    copy_tree(src, dest)
}

fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "node_modules" || name == "out" || name == "target" || name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn build_plugin(dir: &Path) -> Result<()> {
    if which("node").is_none() {
        bail!("node is required to build the VS Code extension (https://nodejs.org)");
    }
    let npm =
        which("npm").ok_or_else(|| anyhow::anyhow!("npm is required to build the extension"))?;
    eprintln!("npm install in {}…", dir.display());
    run_cmd(Command::new(&npm).args(["install"]).current_dir(dir))?;
    eprintln!("tsc…");
    let tsc = dir.join("node_modules/.bin/tsc");
    if tsc.is_file() {
        run_cmd(Command::new(&tsc).current_dir(dir))?;
    } else {
        run_cmd(
            Command::new("npx")
                .args(["--no-install", "tsc"])
                .current_dir(dir),
        )?;
    }
    if !dir.join("out/extension.js").is_file() {
        bail!("extension build produced no out/extension.js");
    }
    Ok(())
}

fn install_unpacked(build: &Path, dest: &Path, hyper_bin: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    fs::create_dir_all(dest)?;
    for name in ["package.json", "README.md", "out", "media"] {
        let from = build.join(name);
        let to = dest.join(name);
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else if from.is_file() {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
    }
    fs::write(dest.join("hyper.bin"), format!("{}\n", hyper_bin.display()))
        .context("write hyper.bin")?;
    Ok(())
}

fn which(name: &str) -> Option<PathBuf> {
    let output = Command::new("sh")
        .args(["-lc", &format!("command -v {name}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawn {cmd:?}"))?;
    if !status.success() {
        bail!("command failed: {cmd:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_layout_in_repo() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/vscode-hyper");
        assert!(looks_like_plugin(&p), "{}", p.display());
        let name = extension_folder_name(&p).expect("package.json");
        assert!(name.starts_with("wu1w.hyper-"), "{name}");
    }
}
