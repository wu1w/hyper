//! Progressive disclosure for skills. Catalog (name + one trigger line) may go
//! in the session system prompt when `skills_auto_catalog` is on. SKILL.md
//! bodies never do, and `skill` is not in `tools[]`.
//!
//! Overlay matches MCP: later wins, missing dirs skipped.
//! Home (low → high): `~/.cursor/skills`, `~/.grok/skills`, then
//! `~/.grok-hyper/skills`. Workspace (low → high): `.cursor/skills`,
//! `.grok/skills`, `.grok-hyper/skills`. Codex `~/.agents` / `.agents` are not
//! scanned (broken YAML descriptions leak into the system prompt). Harness
//! injects at most one body as a hidden user after the live query. A
//! model-emitted `skill` call still hits [`run_skill`] so XML does not fall
//! through to unknown-tool.

use std::path::{Path, PathBuf};

use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::{arg_str, folded_response, BlobStore, ToolLimits};

const MAX_CATALOG_SKILLS: usize = 24;
const MAX_CATALOG_CHARS: usize = 1600;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    pub skills: Vec<Skill>,
}

impl SkillCatalog {
    pub fn load(home: &Path, workspace: &Path) -> Self {
        let mut skills = Vec::new();
        if home
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n == ".grok-hyper")
        {
            if let Some(user) = home.parent() {
                scan_dir(user.join(".cursor").join("skills"), &mut skills);
                scan_dir(user.join(".grok").join("skills"), &mut skills);
            }
        }
        scan_dir(home.join("skills"), &mut skills);
        scan_dir(workspace.join(".cursor").join("skills"), &mut skills);
        scan_dir(workspace.join(".grok").join("skills"), &mut skills);
        scan_dir(workspace.join(".grok-hyper").join("skills"), &mut skills);
        let mut out: Vec<Skill> = Vec::new();
        for sk in skills {
            if skip_vendor_default(&sk.name) {
                continue;
            }
            if let Some(i) = out
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(&sk.name))
            {
                out[i] = sk;
            } else {
                out.push(sk);
            }
        }
        out.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
        Self { skills: out }
    }

    /// One name + trigger line each. Empty when there are no skills.
    /// Workspace skills first; home dumps (dozens of Cursor skills) are capped
    /// so every hop does not pay for the full `~/.cursor/skills` list.
    pub fn catalog_markdown(&self) -> String {
        self.catalog_markdown_for(None)
    }

    pub fn catalog_markdown_for(&self, workspace: Option<&Path>) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut items: Vec<&Skill> = self.skills.iter().collect();
        items.sort_by(|a, b| {
            let a_ws = workspace.is_some_and(|ws| a.path.starts_with(ws));
            let b_ws = workspace.is_some_and(|ws| b.path.starts_with(ws));
            b_ws.cmp(&a_ws).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });
        let mut s = String::from("skills:\n");
        let mut shown = 0usize;
        for sk in items {
            let trig = if sk.description.is_empty() {
                "on demand"
            } else {
                sk.description.trim()
            };
            let trig: String = trig.chars().take(40).collect();
            let line = format!("- {}: {}\n", sk.name, trig);
            if shown >= MAX_CATALOG_SKILLS || s.len() + line.len() > MAX_CATALOG_CHARS {
                break;
            }
            s.push_str(&line);
            shown += 1;
        }
        let rest = self.skills.len().saturating_sub(shown);
        if rest > 0 {
            s.push_str(&format!("… {rest} more; name a skill to load it.\n"));
        }
        s
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        let n = name.trim();
        self.skills.iter().find(|s| s.name.eq_ignore_ascii_case(n))
    }
}

/// Direct SKILL.md load. Not in `tools[]`; bodies normally go through
/// [`hidden_card`] (fail-closed at 400 tok). `dispatch_one` still calls this
/// when the model emits `skill` so the tool_call id gets a result.
pub fn run_skill(
    catalog: &SkillCatalog,
    call: &ToolCall,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(name) = arg_str(&call.arguments, "name") else {
        return ToolResponse::text(&call.id, "Error: skill needs `name`.", ToolState::Error);
    };
    let Some(sk) = catalog.get(&name) else {
        let known: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: unknown skill '{name}'. Known: {}",
                if known.is_empty() {
                    "(none)".into()
                } else {
                    known.join(", ")
                }
            ),
            ToolState::Error,
        );
    };
    if crate::tools::is_special_file(&sk.path) {
        return ToolResponse::text(
            &call.id,
            format!("Error: skill '{name}' is not a regular file."),
            ToolState::Error,
        );
    }
    if crate::tools::is_oversized_text(&sk.path) {
        return ToolResponse::text(
            &call.id,
            format!("Error: skill '{name}' is too large to load."),
            ToolState::Error,
        );
    }
    match crate::tools::read_bytes_regular(&sk.path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(body) => folded_response(&call.id, body, ToolState::Success, limits, blobs),
            Err(_) => ToolResponse::text(
                &call.id,
                format!("Error: skill '{name}' is not valid UTF-8 text."),
                ToolState::Error,
            ),
        },
        Err(e) => ToolResponse::text(
            &call.id,
            format!("Error: {}", crate::tools::io_user_msg(&e)),
            ToolState::Error,
        ),
    }
}

pub fn hidden_card(skill: &Skill) -> Option<String> {
    if crate::tools::is_special_file(&skill.path) || crate::tools::is_oversized_text(&skill.path) {
        return None;
    }
    let raw = crate::tools::read_text_if_regular(&skill.path)?;
    let body = strip_frontmatter(&raw);
    if body.is_empty() {
        return None;
    }
    if crate::sticky::tokens(&body) > crate::sticky::SKILL_BODY_MAX_TOKENS {
        return None;
    }
    Some(format!("[skill: {}]\n{}", skill.name, body))
}

/// At most one skill. Explicit `[skill:name]` / exact name beats FAILED / commit.
pub fn match_user<'a>(catalog: &'a SkillCatalog, user: &str) -> Option<&'a Skill> {
    let (forced, rest) = crate::sticky::split_skill_prefix(user);
    if let Some(name) = forced {
        return catalog.get(&name);
    }
    if let Some(sk) = named_in_text(catalog, rest.as_str()) {
        return Some(sk);
    }
    if commitish(&rest) {
        return catalog
            .get("commit")
            .or_else(|| named_in_text(catalog, "commit"));
    }
    None
}

pub fn match_tool_output<'a>(catalog: &'a SkillCatalog, output: &str) -> Option<&'a Skill> {
    if !tests_failed(output) {
        return None;
    }
    catalog
        .get("testhook")
        .or_else(|| catalog.get("test"))
        .or_else(|| catalog.get("tests"))
}

fn named_in_text<'a>(catalog: &'a SkillCatalog, text: &str) -> Option<&'a Skill> {
    for sk in &catalog.skills {
        if has_name_token(text, &sk.name) {
            return Some(sk);
        }
    }
    None
}

fn has_name_token(hay: &str, name: &str) -> bool {
    if name.chars().any(|c| !c.is_ascii()) {
        return hay.contains(name);
    }
    hay.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|w| w.eq_ignore_ascii_case(name))
}

fn commitish(user: &str) -> bool {
    user.contains("提交")
        || user
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case("commit") || w.eq_ignore_ascii_case("commits"))
}

pub fn tests_failed(text: &str) -> bool {
    text.contains("FAILED")
        || text.contains("test failed")
        || text.contains("failures:")
        || text.contains("error: test failed")
}

fn strip_frontmatter(raw: &str) -> String {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("---") else {
        return t.to_string();
    };
    let rest = rest.trim_start_matches('\n');
    match rest.split_once("\n---") {
        Some((_, body)) => body.trim().to_string(),
        None => t.to_string(),
    }
}

fn skip_vendor_default(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "shell" | "canvas" | "statusline"
    )
}

fn scan_dir(dir: PathBuf, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let skill_md = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            path.clone()
        } else {
            continue;
        };
        if !skill_md.is_file() {
            continue;
        }
        if let Some(sk) = parse_skill(&skill_md) {
            out.push(sk);
        }
    }
}

fn parse_skill(path: &Path) -> Option<Skill> {
    let raw = crate::tools::read_text_if_regular(path)?;
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("skill")
        .to_string();
    let (name, description) = frontmatter(&raw);
    Some(Skill {
        name: name.unwrap_or(dir_name),
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
    })
}

fn frontmatter(raw: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, None);
    };
    let Some((fm, _)) = rest.split_once("\n---") else {
        return (None, None);
    };
    let mut name = None;
    let mut description = None;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().trim_matches('"').to_string());
        }
    }
    (name, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_from_frontmatter() {
        let dir = std::env::temp_dir().join(format!("hyper-sk-{}", uuid::Uuid::new_v4().simple()));
        let skill_dir = dir.join("skills").join("pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: Extract text from PDFs\n---\nUse pdftotext.\n",
        )
        .unwrap();
        let hook = dir.join("skills").join("testhook");
        std::fs::create_dir_all(&hook).unwrap();
        std::fs::write(
            hook.join("SKILL.md"),
            "---\nname: testhook\n---\nRerun the failing file.\n",
        )
        .unwrap();
        let cat = SkillCatalog::load(&dir, &dir);
        assert_eq!(cat.skills.len(), 2);
        assert_eq!(cat.skills[0].name, "pdf");
        assert!(cat.catalog_markdown().contains("pdf"));
        assert!(cat.catalog_markdown().starts_with("skills:"));
        let card = hidden_card(&cat.skills[0]).unwrap();
        assert!(card.starts_with("[skill: pdf]"));
        assert!(card.contains("Use pdftotext"));
        assert!(match_user(&cat, "Call pdf on this file").is_some());
        assert!(match_user(&cat, "这个函数 off-by-one 吗").is_none());
        assert!(match_tool_output(&cat, "test FAILED in foo.rs").is_some());
        let call = crate::tool_calls::ToolCall {
            id: "1".into(),
            name: "skill".into(),
            arguments: serde_json::json!({"name": "pdf"}),
        };
        let loaded = run_skill(&cat, &call, crate::tools::ToolLimits::default(), None);
        assert!(loaded.joined_text().contains("Use pdftotext"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catalog_markdown_caps_home_dump_and_prefers_workspace() {
        let ws = PathBuf::from("/tmp/hyper-ws-skills");
        let mut skills = Vec::new();
        for i in 0..40 {
            skills.push(Skill {
                name: format!("home-{i:02}"),
                description: "x".repeat(40),
                path: PathBuf::from(format!("/home/u/.cursor/skills/home-{i:02}/SKILL.md")),
            });
        }
        skills.push(Skill {
            name: "proj-skill".into(),
            description: "workspace only".into(),
            path: ws.join(".cursor/skills/proj-skill/SKILL.md"),
        });
        let cat = SkillCatalog { skills };
        let md = cat.catalog_markdown_for(Some(&ws));
        assert!(md.starts_with("skills:"), "{md}");
        assert!(md.contains("proj-skill"), "{md}");
        let proj_at = md.find("proj-skill").unwrap();
        let home_at = md.find("home-").unwrap_or(md.len());
        assert!(proj_at < home_at, "workspace skill must list first:\n{md}");
        assert!(md.contains("more; name a skill to load it"), "{md}");
        assert!(md.len() <= MAX_CATALOG_CHARS + 80, "len={}", md.len());
        let listed = md.lines().filter(|l| l.starts_with("- ")).count();
        assert!(listed <= MAX_CATALOG_SKILLS, "listed={listed}");
    }

    #[cfg(unix)]
    #[test]
    fn run_skill_fifo_is_error_not_hang() {
        let dir =
            std::env::temp_dir().join(format!("hyper-sk-fifo-{}", uuid::Uuid::new_v4().simple()));
        let skill_dir = dir.join("skills").join("pipe");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pipe\ndescription: tmp\n---\nbody\n",
        )
        .unwrap();
        let cat = SkillCatalog::load(&dir, &dir);
        let fifo = skill_dir.join("SKILL.md");
        std::fs::remove_file(&fifo).unwrap();
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let call = crate::tool_calls::ToolCall {
            id: "1".into(),
            name: "skill".into(),
            arguments: serde_json::json!({"name": "pipe"}),
        };
        let started = std::time::Instant::now();
        let loaded = run_skill(&cat, &call, crate::tools::ToolLimits::default(), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO skill must not block: {:?}",
            started.elapsed()
        );
        assert_eq!(loaded.state, ToolState::Error);
        assert!(
            loaded.joined_text().contains("not a regular file"),
            "{}",
            loaded.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn catalog_skips_fifo_skill_md() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-sk-cat-fifo-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let skill_dir = dir.join("skills").join("pipe");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let fifo = skill_dir.join("SKILL.md");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let cat = SkillCatalog::load(&dir, &dir);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO SKILL.md catalog must not block: {:?}",
            started.elapsed()
        );
        assert!(cat.get("pipe").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workspace_skill_overlays_home() {
        let root =
            std::env::temp_dir().join(format!("hyper-sk-ov-{}", uuid::Uuid::new_v4().simple()));
        let home = root.join("home");
        let ws = root.join("ws");
        let home_pdf = home.join("skills").join("pdf");
        let ws_pdf = ws.join(".grok-hyper").join("skills").join("pdf");
        let home_only = home.join("skills").join("commit");
        std::fs::create_dir_all(&home_pdf).unwrap();
        std::fs::create_dir_all(&ws_pdf).unwrap();
        std::fs::create_dir_all(&home_only).unwrap();
        std::fs::write(
            home_pdf.join("SKILL.md"),
            "---\nname: pdf\ndescription: home pdf\n---\nhome body\n",
        )
        .unwrap();
        std::fs::write(
            ws_pdf.join("SKILL.md"),
            "---\nname: pdf\ndescription: workspace pdf\n---\nworkspace body\n",
        )
        .unwrap();
        std::fs::write(
            home_only.join("SKILL.md"),
            "---\nname: commit\n---\ncommit body\n",
        )
        .unwrap();
        let cat = SkillCatalog::load(&home, &ws);
        assert_eq!(cat.skills.len(), 2);
        let pdf = cat.get("pdf").unwrap();
        assert_eq!(pdf.description, "workspace pdf");
        assert!(
            hidden_card(pdf).unwrap().contains("workspace body"),
            "{:?}",
            hidden_card(pdf)
        );
        assert!(cat.get("commit").is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cursor_grok_agents_dirs_overlay_and_skip_vendor_defaults() {
        let root =
            std::env::temp_dir().join(format!("hyper-sk-paths-{}", uuid::Uuid::new_v4().simple()));
        let user = root.join("user");
        let home = user.join(".grok-hyper");
        let ws = root.join("ws");
        let write = |dir: &std::path::Path, name: &str, desc: &str| {
            let skill_dir = dir.join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {desc}\n---\nbody\n"),
            )
            .unwrap();
        };
        write(&user.join(".cursor").join("skills"), "shell", "vendor");
        write(&user.join(".cursor").join("skills"), "canvas", "vendor");
        write(&user.join(".cursor").join("skills"), "statusline", "vendor");
        write(&user.join(".cursor").join("skills"), "pdf", "cursor home");
        write(&user.join(".grok").join("skills"), "pdf", "grok home");
        write(&ws.join(".agents").join("skills"), "review", "ws agents");
        write(&ws.join(".cursor").join("skills"), "review", "ws cursor");
        write(&ws.join(".grok").join("skills"), "pdf", "workspace grok");
        let cat = SkillCatalog::load(&home, &ws);
        assert!(cat.get("shell").is_none());
        assert!(cat.get("canvas").is_none());
        assert!(cat.get("statusline").is_none());
        assert_eq!(cat.get("pdf").unwrap().description, "workspace grok");
        assert_eq!(cat.get("review").unwrap().description, "ws cursor");
        assert_ne!(
            cat.get("review").unwrap().description,
            "ws agents",
            "Codex .agents/skills must not be scanned"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
