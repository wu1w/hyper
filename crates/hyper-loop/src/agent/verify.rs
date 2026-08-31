//! Result-driven test oracle: run a **scoped** suite after a code edit, not
//! because the user said "fix". Office docs / plan.md never trigger this.
//!
//! Commands stay cheap: `cargo test -p <pkg> --lib` or `pytest` on related
//! files. Full-workspace `cargo test` is never the default.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::tool_calls::{CancelFlag, ToolState};
use serde_json::Value;

const DIAG_TIMEOUT: Duration = Duration::from_secs(12);
const DIAG_MAX: usize = 2000;

const CODE_EXT: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "kt", "kts", "c", "cc",
    "cpp", "cxx", "h", "hpp", "cs", "rb", "swift", "scala", "php", "zig",
];

pub fn is_code_path(path: &str) -> bool {
    let p = normalize(path);
    if is_overlay_path(&p) {
        return false;
    }
    let Some((_, ext)) = p.rsplit_once('.') else {
        return false;
    };
    CODE_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Workspace overlay (overnight scripts, todos, session-local files) is not
/// product source. Cargo/ruff/[refs] on it is think waste.
fn is_overlay_path(p: &str) -> bool {
    p == ".grok-hyper"
        || p.starts_with(".grok-hyper/")
        || p.contains("/.grok-hyper/")
        || p.ends_with("/.grok-hyper")
}

/// tests/ or test/ directory component, or a test-named file.
pub fn is_test_path(path: &str) -> bool {
    let p = normalize(path).to_lowercase();
    let mut parts = p.split('/').peekable();
    let mut file = "";
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            file = part;
        } else if part == "tests" || part == "test" || part == "__tests__" {
            return true;
        }
    }
    file.starts_with("test_")
        || file == "tests.rs"
        || file == "tests.ts"
        || file == "tests.js"
        || file == "tests.py"
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_tests.rs")
}

fn normalize(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
}

/// Workspace has some way to run tests. Used for `--print` baseline only.
pub fn workspace_has_tests(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
        || root.join("pyproject.toml").is_file()
        || root.join("pytest.ini").is_file()
        || root.join("tests").is_dir()
        || has_root_python_tests(root)
}

/// Best-effort command for the files just edited. `None` = do not auto-run.
pub fn scoped_test_cmd(root: &Path, edited: &[String]) -> Option<String> {
    let code: Vec<String> = edited
        .iter()
        .map(|p| normalize(p))
        .filter(|p| is_code_path(p) && !is_test_path(p))
        .collect();
    if code.is_empty() {
        return None;
    }
    if let Some(cmd) = cargo_scoped(root, &code) {
        return Some(cmd);
    }
    if let Some(cmd) = pytest_scoped(root, &code) {
        return Some(cmd);
    }
    python_unittest(root)
}

/// Compile/lint after a successful Write/StrReplace. `None` = clean or nothing
/// to attach (timeouts stay off this path so a Write is not tagged `[diagnostics]`
/// for a checker that never finished).
/// Runs on a blocking thread so `cargo check` cannot stall the agent runtime.
pub async fn run_diagnostics_async(
    root: &Path,
    edited: &[String],
    cancel: &CancelFlag,
) -> Option<String> {
    match run_lints_async(root, edited, cancel).await {
        LintReport::Findings(s) => Some(truncate_diag(format!("[diagnostics]\n{s}"))),
        _ => None,
    }
}

pub async fn run_lints_async(root: &Path, edited: &[String], cancel: &CancelFlag) -> LintReport {
    let root = root.to_path_buf();
    let edited = edited.to_vec();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || run_lints(&root, &edited, &cancel))
        .await
        .unwrap_or(LintReport::Cancelled)
}

/// Honest ReadLints / cargo-check outcome. Timeout and "never ran" are not Clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintReport {
    Clean,
    Findings(String),
    Incomplete(String),
    Cancelled,
}

pub fn read_lints_reply(report: &LintReport, paths: &[String]) -> (String, ToolState) {
    match report {
        LintReport::Clean => (
            format!(
                "No compiler or linter errors found for: {}",
                paths.join(", ")
            ),
            ToolState::Success,
        ),
        LintReport::Findings(s) => (
            truncate_diag(format!("[diagnostics]\n{s}")),
            ToolState::Success,
        ),
        LintReport::Incomplete(s) => (s.clone(), ToolState::Success),
        LintReport::Cancelled => ("Error: lint check aborted".into(), ToolState::Interrupted),
    }
}

fn truncate_diag(mut out: String) -> String {
    if out.len() > DIAG_MAX {
        out.truncate(DIAG_MAX);
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
    }
    out
}

pub fn run_diagnostics(root: &Path, edited: &[String], cancel: &CancelFlag) -> Option<String> {
    match run_lints(root, edited, cancel) {
        LintReport::Findings(s) => Some(truncate_diag(format!("[diagnostics]\n{s}"))),
        _ => None,
    }
}

enum Check {
    Clean,
    Findings(String),
    Timeout(&'static str),
    Skip(String),
    Cancelled,
}

fn merge_checks(checks: Vec<Check>) -> LintReport {
    let mut findings = Vec::new();
    let mut notes = Vec::new();
    let mut ran = false;
    for c in checks {
        match c {
            Check::Cancelled => return LintReport::Cancelled,
            Check::Findings(s) => {
                ran = true;
                findings.push(s);
            }
            Check::Clean => ran = true,
            Check::Timeout(what) => notes.push(format!(
                "{what} did not finish in {}s. Not a compiler verdict — the crate was too large for this hop. Do not assume the code is broken.",
                DIAG_TIMEOUT.as_secs()
            )),
            Check::Skip(why) => notes.push(why),
        }
    }
    if !findings.is_empty() {
        let mut s = findings.join("\n");
        if !notes.is_empty() {
            s.push('\n');
            s.push_str(&notes.join("\n"));
        }
        return LintReport::Findings(s);
    }
    if !notes.is_empty() {
        return LintReport::Incomplete(notes.join("\n"));
    }
    if ran {
        LintReport::Clean
    } else {
        LintReport::Incomplete("no compiler or linter ran for these paths".into())
    }
}

pub fn run_lints(root: &Path, edited: &[String], cancel: &CancelFlag) -> LintReport {
    if cancel.is_cancelled() {
        return LintReport::Cancelled;
    }
    let code: Vec<String> = edited
        .iter()
        .map(|p| normalize(p))
        .filter(|p| is_code_path(p))
        .collect();
    if code.is_empty() {
        return LintReport::Incomplete("no code files to check".into());
    }
    let mut checks = Vec::new();
    if code.iter().any(|p| p.ends_with(".rs")) {
        checks.push(cargo_check(root, &code, cancel));
    }
    if has_ts_path(&code) {
        checks.push(tsc_check(root, &code, cancel));
    }
    if code.iter().any(|p| p.ends_with(".py")) {
        checks.push(ruff_check(root, &code, cancel));
    }
    merge_checks(checks)
}

fn cargo_check(root: &Path, edited: &[String], cancel: &CancelFlag) -> Check {
    let rust: Vec<&str> = edited
        .iter()
        .map(|s| s.as_str())
        .filter(|p| p.ends_with(".rs"))
        .collect();
    if rust.is_empty() {
        return Check::Skip("no Rust files".into());
    }
    let Some((_, name)) = nearest_cargo_package(root, rust[0]) else {
        return Check::Skip(format!("no Cargo.toml package for {}", rust[0]));
    };
    match run_cmd(
        "cargo",
        &[
            "check",
            "-p",
            &name,
            "--message-format=json",
            "--color",
            "never",
        ],
        root,
        cancel,
    ) {
        CmdFinish::Cancelled => Check::Cancelled,
        CmdFinish::Timeout => Check::Timeout("cargo check"),
        CmdFinish::Failed => Check::Skip("cargo check could not start".into()),
        CmdFinish::Output(stdout) => {
            let mut errors = Vec::new();
            for line in stdout.lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if v["reason"].as_str() != Some("compiler-message") {
                    continue;
                }
                let msg = &v["message"];
                if msg["level"].as_str() != Some("error") {
                    continue;
                }
                if let Some(rendered) = msg["rendered"].as_str() {
                    let t = rendered.trim();
                    if !t.is_empty() && !errors.iter().any(|e: &String| e == t) {
                        errors.push(t.to_string());
                    }
                } else if let Some(m) = msg["message"].as_str() {
                    if !errors.iter().any(|e: &String| e == m) {
                        errors.push(m.to_string());
                    }
                }
            }
            if errors.is_empty() {
                Check::Clean
            } else {
                Check::Findings(errors.join("\n"))
            }
        }
    }
}

fn has_ts_path(edited: &[String]) -> bool {
    edited.iter().any(|p| is_ts_path(p))
}

fn is_ts_path(p: &str) -> bool {
    matches!(
        p.rsplit('.').next().unwrap_or(""),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"
    )
}

pub(crate) fn nearest_tsconfig(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut dir = root.join(rel);
    if dir.is_file() || dir.extension().is_some() {
        dir.pop();
    }
    loop {
        if dir.join("tsconfig.json").is_file() {
            return Some(dir);
        }
        if dir == *root || !dir.pop() {
            break;
        }
    }
    None
}

fn shown_dir(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .map(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            if s.is_empty() {
                ".".into()
            } else {
                s
            }
        })
        .unwrap_or_else(|_| dir.display().to_string())
}

fn tsc_bin(dir: &Path, root: &Path) -> Option<PathBuf> {
    for base in [dir, root] {
        let unix = base.join("node_modules/.bin/tsc");
        if unix.is_file() {
            return Some(unix);
        }
        #[cfg(windows)]
        {
            let cmd = base.join("node_modules/.bin/tsc.cmd");
            if cmd.is_file() {
                return Some(cmd);
            }
        }
    }
    None
}

fn tsc_check(root: &Path, edited: &[String], cancel: &CancelFlag) -> Check {
    let ts_files: Vec<&str> = edited
        .iter()
        .map(|s| s.as_str())
        .filter(|p| is_ts_path(p))
        .collect();
    if ts_files.is_empty() {
        return Check::Skip("no TypeScript files".into());
    }
    let mut by_dir: HashMap<PathBuf, Vec<&str>> = HashMap::new();
    let mut unconfigured = Vec::new();
    for p in &ts_files {
        match nearest_tsconfig(root, p) {
            Some(dir) => by_dir.entry(dir).or_default().push(*p),
            None => unconfigured.push(*p),
        }
    }
    let mut findings = Vec::new();
    let mut notes = Vec::new();
    let mut ran = false;
    for (dir, _) in &by_dir {
        if cancel.is_cancelled() {
            return Check::Cancelled;
        }
        let rel = shown_dir(root, dir);
        let Some(bin) = tsc_bin(dir, root) else {
            notes.push(format!(
                "tsc not found for {rel} (no node_modules/.bin/tsc). That is not a clean TypeScript check."
            ));
            continue;
        };
        match run_cmd(
            &bin.to_string_lossy(),
            &["--noEmit", "--pretty", "false"],
            dir,
            cancel,
        ) {
            CmdFinish::Cancelled => return Check::Cancelled,
            CmdFinish::Timeout => notes.push(format!(
                "tsc did not finish in {}s in {rel}. Not a compiler verdict — the project was too large for this hop. Do not assume the code is broken.",
                DIAG_TIMEOUT.as_secs()
            )),
            CmdFinish::Failed => {
                notes.push(format!("tsc could not start in {rel}"));
            }
            CmdFinish::Output(out) => {
                ran = true;
                let err: String = out
                    .lines()
                    .filter(|l| l.contains("error TS"))
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !err.trim().is_empty() {
                    findings.push(err);
                }
            }
        }
    }
    for p in unconfigured {
        notes.push(format!(
            "no tsconfig.json for {p} (looked in parent directories). That is not a clean TypeScript check."
        ));
    }
    if !findings.is_empty() {
        let mut s = findings.join("\n");
        if !notes.is_empty() {
            s.push('\n');
            s.push_str(&notes.join("\n"));
        }
        return Check::Findings(s);
    }
    if !notes.is_empty() {
        return Check::Skip(notes.join("\n"));
    }
    if ran {
        Check::Clean
    } else {
        Check::Skip("TypeScript checker did not run".into())
    }
}

fn ruff_check(root: &Path, edited: &[String], cancel: &CancelFlag) -> Check {
    let py: Vec<&str> = edited
        .iter()
        .map(|s| s.as_str())
        .filter(|p| p.ends_with(".py"))
        .collect();
    if py.is_empty() {
        return Check::Skip("no Python files".into());
    }
    if !cmd_exists("ruff") {
        return Check::Skip("ruff not found on PATH. That is not a clean Python check.".into());
    }
    let mut args = vec!["check", "--quiet"];
    args.extend(py.iter().copied());
    match run_cmd("ruff", &args, root, cancel) {
        CmdFinish::Cancelled => Check::Cancelled,
        CmdFinish::Timeout => Check::Timeout("ruff"),
        CmdFinish::Failed => Check::Skip("ruff could not start".into()),
        CmdFinish::Output(out) => {
            let t = out.trim();
            if t.is_empty() {
                Check::Clean
            } else {
                Check::Findings(t.to_string())
            }
        }
    }
}

fn cmd_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

enum CmdFinish {
    Output(String),
    Timeout,
    Cancelled,
    Failed,
}

fn run_cmd(prog: &str, args: &[&str], cwd: &Path, cancel: &CancelFlag) -> CmdFinish {
    let mut cmd = Command::new(prog);
    crate::proc_spawn::hide_window(&mut cmd);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if prog == "cargo" || prog.ends_with("cargo") || prog.ends_with("cargo.exe") {
        cmd.env("CARGO_TARGET_DIR", cwd.join("target"));
        cmd.env("CARGO_TERM_COLOR", "never");
        cmd.env("CARGO_INCREMENTAL", "0");
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return CmdFinish::Failed,
    };
    let started = Instant::now();
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return CmdFinish::Cancelled;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                let mut buf = Vec::new();
                if let Some(out) = child.stdout.as_mut() {
                    let _ = out.read_to_end(&mut buf);
                }
                if buf.is_empty() {
                    if let Some(err) = child.stderr.as_mut() {
                        let _ = err.read_to_end(&mut buf);
                    }
                }
                return CmdFinish::Output(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(None) if started.elapsed() > DIAG_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return CmdFinish::Timeout;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(_) => {
                let _ = child.kill();
                return CmdFinish::Failed;
            }
        }
    }
}

/// Start-of-turn baseline when we do not yet know which files will change.
pub fn workspace_default_test_cmd(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        if package_name(&root.join("Cargo.toml")).is_some() && root.join("src/lib.rs").is_file() {
            let name = package_name(&root.join("Cargo.toml"))?;
            return Some(format!("cargo test -p {name} --lib"));
        }
        return None;
    }
    pytest_scoped(root, &[]).or_else(|| python_unittest(root))
}

fn cargo_scoped(root: &Path, edited: &[String]) -> Option<String> {
    if !root.join("Cargo.toml").is_file() {
        return None;
    }
    let path = edited.first()?;
    let (dir, name) = nearest_cargo_package(root, path)?;
    if dir.join("src/lib.rs").is_file() {
        Some(format!("cargo test -p {name} --lib"))
    } else {
        Some(format!("cargo test -p {name}"))
    }
}

fn nearest_cargo_package(root: &Path, rel: &str) -> Option<(PathBuf, String)> {
    let mut dir = root.join(rel);
    if dir.is_file() {
        dir.pop();
    }
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file() {
            if let Some(name) = package_name(&cargo) {
                return Some((dir, name));
            }
        }
        if dir == root || !dir.pop() {
            break;
        }
    }
    None
}

fn package_name(cargo_toml: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(cargo_toml).ok()?;
    let v: toml::Value = raw.parse().ok()?;
    v.get("package")?
        .get("name")?
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn pytest_scoped(root: &Path, edited: &[String]) -> Option<String> {
    // Only claim pytest when the tree actually looks like a pytest project.
    // A lone `test_*.py` next to an edit is stdlib unittest — pytest may be
    // missing, and `-q` output would then never match the green→red detector.
    let pyish = root.join("pyproject.toml").is_file()
        || root.join("pytest.ini").is_file()
        || root.join("setup.cfg").is_file()
        || root.join("tests").is_dir();
    if !pyish {
        return None;
    }
    let py: Vec<&str> = edited
        .iter()
        .map(|s| s.as_str())
        .filter(|p| p.ends_with(".py"))
        .collect();
    if py.is_empty() && !root.join("tests").is_dir() && !root.join("pytest.ini").is_file() {
        return None;
    }
    let pybin = python_launcher();
    if py.is_empty() {
        return Some(format!("{pybin} -B -m pytest -q tests"));
    }
    let mut targets: Vec<String> = Vec::new();
    for p in &py {
        if let Some(t) = related_pytest(root, p) {
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
    }
    if targets.is_empty() {
        if let Some(parent) = Path::new(py[0]).parent() {
            let s = parent.to_string_lossy().replace('\\', "/");
            if !s.is_empty() && s != "." {
                targets.push(s);
            }
        }
    }
    if targets.is_empty() {
        return Some(format!("{pybin} -B -m pytest -q"));
    }
    Some(format!("{pybin} -B -m pytest -q {}", targets.join(" ")))
}

fn related_pytest(root: &Path, rel: &str) -> Option<String> {
    let p = Path::new(rel);
    let stem = p.file_stem()?.to_string_lossy();
    let parent = p.parent().unwrap_or(Path::new(""));
    let candidates = [
        parent.join(format!("test_{stem}.py")),
        parent.join("tests").join(format!("test_{stem}.py")),
        PathBuf::from("tests").join(format!("test_{stem}.py")),
        parent.join(format!("{stem}_test.py")),
    ];
    for c in candidates {
        if root.join(&c).is_file() {
            return Some(c.to_string_lossy().replace('\\', "/"));
        }
    }
    None
}

fn python_unittest(root: &Path) -> Option<String> {
    let python = python_launcher();
    if root.join("tests").is_dir() {
        return Some(format!("{python} -B -m unittest discover -s tests -v"));
    }
    has_root_python_tests(root)
        .then(|| format!("{python} -B -m unittest discover -s . -p \"test*.py\" -v"))
}

fn has_root_python_tests(root: &Path) -> bool {
    std::fs::read_dir(root).ok().is_some_and(|it| {
        it.filter_map(|e| e.ok()).any(|e| {
            let s = e.file_name().to_string_lossy().into_owned();
            s.starts_with("test_") && s.ends_with(".py")
        })
    })
}

pub fn python_launcher() -> &'static str {
    #[cfg(windows)]
    {
        if std::process::Command::new("py")
            .args(["-3", "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            "py -3"
        } else {
            "python"
        }
    }
    #[cfg(not(windows))]
    {
        "python3"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::{CancelFlag, ToolState};

    #[test]
    fn code_path_skips_office_docs() {
        assert!(is_code_path("src/foo.rs"));
        assert!(is_code_path("pkg/a.py"));
        assert!(!is_code_path("notes.md"));
        assert!(!is_code_path("drafts/memo.txt"));
        assert!(!is_code_path("config.example.toml"));
        assert!(!is_code_path(".grok-hyper/overnight/score_all.py"));
        assert!(!is_code_path(
            "/Users/william/grok-hyper/.grok-hyper/overnight/a.py"
        ));
        assert!(is_code_path("pkg/a.py"));
    }

    #[test]
    fn cargo_scoped_uses_package_lib() {
        let dir =
            std::env::temp_dir().join(format!("hyper-verify-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("crates/pack/src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/pack\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("crates/pack/Cargo.toml"),
            "[package]\nname = \"pack\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("crates/pack/src/lib.rs"), "pub fn f() {}\n").unwrap();
        let cmd = scoped_test_cmd(&dir, &["crates/pack/src/lib.rs".into()]).unwrap();
        assert_eq!(cmd, "cargo test -p pack --lib");
        assert!(scoped_test_cmd(&dir, &["README.md".into()]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pytest_picks_related_file() {
        let dir = std::env::temp_dir().join(format!("hyper-py-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("mod.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(dir.join("tests/test_mod.py"), "def test_f():\n    pass\n").unwrap();
        let cmd = scoped_test_cmd(&dir, &["mod.py".into()]).unwrap();
        assert!(cmd.contains("pytest"), "{cmd}");
        assert!(cmd.contains("tests/test_mod.py"), "{cmd}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unittest_tree_does_not_claim_pytest() {
        let dir = std::env::temp_dir().join(format!("hyper-ut-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
        std::fs::write(dir.join("test_app.py"), "import unittest\n").unwrap();
        let cmd = scoped_test_cmd(&dir, &["app.py".into()]).unwrap();
        assert!(cmd.contains("unittest"), "{cmd}");
        assert!(!cmd.contains("pytest"), "{cmd}");
        assert!(workspace_has_tests(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cargo_check_errors_append_diagnostics() {
        use std::process::{Command, Stdio};
        if Command::new("cargo")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .is_none_or(|s| !s.success())
        {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("hyper-diag-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"hyperdiag\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() -> i32 { \"nope\" }\n").unwrap();
        let cancel = CancelFlag::new();
        let got = run_diagnostics(&dir, &["src/lib.rs".into()], &cancel).unwrap();
        assert!(got.contains("[diagnostics]"), "{got}");
        assert!(
            got.contains("mismatched types") || got.contains("error"),
            "{got}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clean_rust_edit_appends_nothing() {
        use std::process::{Command, Stdio};
        if Command::new("cargo")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .is_none_or(|s| !s.success())
        {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("hyper-diag-ok-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"hyperdiagok\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();
        let cancel = CancelFlag::new();
        assert!(run_diagnostics(&dir, &["src/lib.rs".into()], &cancel).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn timeout_and_skip_are_not_clean() {
        let timeout = format!(
            "cargo check did not finish in {}s. Not a compiler verdict — the crate was too large for this hop. Do not assume the code is broken.",
            DIAG_TIMEOUT.as_secs()
        );
        assert_eq!(
            merge_checks(vec![Check::Timeout("cargo check")]),
            LintReport::Incomplete(timeout.clone())
        );
        assert_eq!(
            merge_checks(vec![Check::Clean, Check::Timeout("cargo check")]),
            LintReport::Incomplete(timeout.clone())
        );
        assert_eq!(merge_checks(vec![Check::Clean]), LintReport::Clean);
        let skip = merge_checks(vec![Check::Skip(
            "no tsconfig.json for App.tsx (looked in parent directories). That is not a clean TypeScript check.".into(),
        )]);
        match skip {
            LintReport::Incomplete(s) => assert!(s.contains("App.tsx"), "{s}"),
            other => panic!("{other:?}"),
        }
        let (text, state) = read_lints_reply(&LintReport::Incomplete(timeout), &["find.rs".into()]);
        assert_eq!(state, ToolState::Success);
        assert!(!text.starts_with("Error:"), "{text}");
        assert!(text.contains("Not a compiler verdict"), "{text}");
        assert!(!text.contains("No compiler or linter errors"), "{text}");
        let (ok, st) = read_lints_reply(&LintReport::Clean, &["src/lib.rs".into()]);
        assert_eq!(st, ToolState::Success);
        assert!(ok.contains("No compiler or linter errors"), "{ok}");
    }

    #[test]
    fn nearest_tsconfig_walks_from_the_file() {
        let dir =
            std::env::temp_dir().join(format!("hyper-tsconfig-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("web/console/src")).unwrap();
        std::fs::write(dir.join("web/console/tsconfig.json"), "{}\n").unwrap();
        std::fs::write(dir.join("web/console/src/App.tsx"), "export {}\n").unwrap();
        let found = nearest_tsconfig(&dir, "web/console/src/App.tsx").unwrap();
        assert_eq!(found, dir.join("web/console"));
        assert!(nearest_tsconfig(&dir, "crates/hyper-loop/src/lib.rs").is_none());
        let report = run_lints(
            &dir,
            &[
                "crates/hyper-loop/src/find.rs".into(),
                "web/console/src/App.tsx".into(),
            ],
            &CancelFlag::new(),
        );
        match report {
            LintReport::Incomplete(s) => {
                assert!(
                    s.contains("App.tsx") || s.contains("tsc not found") || s.contains("no Cargo"),
                    "{s}"
                );
                assert!(!s.contains("No compiler or linter errors"), "{s}");
            }
            LintReport::Findings(s) => {
                assert!(
                    s.contains("tsc") || s.contains("Cargo") || s.contains("App.tsx"),
                    "{s}"
                );
            }
            other => panic!("mixed rust+ts without root tsconfig must not look clean: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
