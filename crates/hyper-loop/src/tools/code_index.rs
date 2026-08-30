//! Workspace code index. `Search` covers Cursor's glob / exact-symbol /
//! keyword paths and returns function-sized spans, not grep dumps. Not in
//! the core tool set — `bind_periphery` appends `search_tool()` when `code_search` is on.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::{params, Connection, Transaction};

use super::{arg_str, folded_response, ToolLimits, Workspace};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

/// Same copy for empty index and "index not bound yet" so the model retries
/// Search instead of falling through to Glob/Grep.
pub const SEARCH_WARMING: &str =
    "No matches. Index is empty or still warming. Retry Search shortly, or Read a path you already know.";

const HIT_CAP: usize = 8;
const CHUNK_LINES: usize = 80;
const RENDER_CHARS: usize = 4000;
const MAX_FILES: usize = 50_000;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const INDEX_SCHEMA: i64 = 4;
/// Agent::new rebuilds the index every turn. A persistent sqlite that was
/// scanned moments ago is still current; skip git-ls + metadata until this
/// window elapses so Windows doesn't re-walk tens of thousands of files.
const RESCAN_EVERY: Duration = Duration::from_secs(12);
/// Hard cap for the first-hop scan. Windows Defender + Documents/Desktop as
/// the workspace used to block "等待模型" for minutes before any HTTP call.
/// Git-tracked trees are bounded by `git ls-files`; give them more time so
/// Search is dense enough that grok-4.6 does not fall through to Grep storms.
const INDEX_BUDGET: Duration = Duration::from_secs(3);
const INDEX_BUDGET_GIT: Duration = Duration::from_secs(8);
const GIT_TIMEOUT: Duration = Duration::from_secs(4);
static LAST_FULL_SCAN: Mutex<Option<(PathBuf, Instant)>> = Mutex::new(None);

fn scan_key(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn reuse_persistent(root: &Path) -> bool {
    let key = scan_key(root);
    let Ok(g) = LAST_FULL_SCAN.lock() else {
        return false;
    };
    g.as_ref()
        .is_some_and(|(p, t)| p == &key && t.elapsed() < RESCAN_EVERY)
}

fn mark_scanned(root: &Path) {
    if let Ok(mut g) = LAST_FULL_SCAN.lock() {
        *g = Some((scan_key(root), Instant::now()));
    }
}

const SKIP_DIR: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    "third_party",
    "__pycache__",
    ".venv",
    "venv",
    "blobs",
    "AppData",
    "Application Data",
    "Local Settings",
    "Library",
    "Caches",
    "OneDrive",
    "Downloads",
    "Dropbox",
];

/// Gitignored workspace overlay. Overnight scripts stay Search-visible;
/// session dumps / blob archives do not.
const HYPER_SKIP_DIR: &[&str] = &["sessions", "blobs", "doc-cache", "generated"];

pub struct CodeIndex {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub(crate) struct Hit {
    path: String,
    start: u32,
    end: u32,
    body: String,
}

impl CodeIndex {
    pub fn build(root: &Path) -> Self {
        // Packaged Electron used to spawn with cwd=home. Indexing a Windows
        // profile (AppData / OneDrive / Downloads) blocks the first model hop
        // for minutes while the UI says "正在调用模型".
        if skip_index_root(root) {
            return Self::empty();
        }
        if reuse_persistent(root) {
            if let Some(idx) = Self::persistent(root) {
                return idx;
            }
        }
        let tracked = git_ls_files(root);
        let budget = ScanBudget::new(if tracked.is_some() {
            INDEX_BUDGET_GIT
        } else {
            INDEX_BUDGET
        });
        if budget.exceeded() {
            return Self::persistent(root).unwrap_or_else(Self::empty);
        }
        let mut files = tracked
            .clone()
            .unwrap_or_else(|| walk_fallback(root, &budget));
        if tracked.is_some() && !budget.exceeded() {
            files.extend(collect_nested_git_files(root, &budget));
            files.extend(collect_hyper_overlay_files(root, &budget));
            files.sort();
            files.dedup();
        }
        // Git workspaces get a global cache. Scratch/non-repository folders
        // stay in memory, so hyper never leaves project-local index artifacts.
        let idx = if tracked.is_some() {
            Self::persistent(root).unwrap_or_else(Self::empty)
        } else {
            Self::empty()
        };
        idx.sync_root(root, files, &budget);
        if tracked.is_some() && !budget.exceeded() {
            mark_scanned(root);
        }
        idx
    }

    pub(crate) fn is_empty(&self) -> bool {
        let conn = crate::lock_unpoison(&self.conn);
        conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0)
            == 0
    }

    fn empty() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        init_schema(&conn).expect("fts5 chunks");
        Self {
            conn: Mutex::new(conn),
        }
    }

    fn persistent(root: &Path) -> Option<Self> {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let key = crate::vendor::sha256_hex(canonical.to_string_lossy().as_bytes());
        let dir = crate::config::Config::home_dir().ok()?.join("code-index");
        std::fs::create_dir_all(&dir).ok()?;
        let conn = Connection::open(dir.join(format!("{}.sqlite3", &key[..24]))).ok()?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        init_schema(&conn).ok()?;
        Some(Self {
            conn: Mutex::new(conn),
        })
    }

    fn sync_root(&self, root: &Path, files: Vec<PathBuf>, budget: &ScanBudget) {
        let known = {
            let conn = crate::lock_unpoison(&self.conn);
            let Ok(mut stmt) = conn.prepare("SELECT path, size, mtime_ns FROM files") else {
                return;
            };
            let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            }) else {
                return;
            };
            rows.flatten()
                .map(|(p, size, mtime)| (p, (size, mtime)))
                .collect::<HashMap<_, _>>()
        };

        let mut seen = HashSet::new();
        let mut updates = Vec::new();
        for rel in files.into_iter().take(MAX_FILES) {
            if budget.exceeded() {
                break;
            }
            if !should_index(&rel) {
                continue;
            }
            let abs = root.join(&rel);
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            if !meta.is_file() || meta.len() as usize > MAX_FILE_BYTES {
                continue;
            }
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let stamp = file_stamp(&meta);
            seen.insert(rel_s.clone());
            if known.get(&rel_s) == Some(&stamp) {
                continue;
            }
            let content = std::fs::read_to_string(&abs)
                .ok()
                .filter(|s| !s.contains('\0') && s.len() <= MAX_FILE_BYTES);
            updates.push((rel_s, stamp, content));
        }

        let stale: Vec<String> = known
            .keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect();
        let mut conn = crate::lock_unpoison(&self.conn);
        let Ok(tx) = conn.transaction() else {
            return;
        };
        for path in stale {
            drop_path_tx(&tx, &path);
        }
        for (path, stamp, content) in updates {
            if let Some(content) = content {
                upsert_file_tx(&tx, &path, &content, stamp);
            } else {
                drop_path_tx(&tx, &path);
            }
        }
        let _ = tx.commit();
    }

    pub fn refresh(&self, ws: &Workspace, raw_path: &str) {
        let shown = ws.shown(raw_path);
        let Ok(abs) = ws.resolve(raw_path) else {
            self.drop_path(&shown);
            return;
        };
        if !should_index(Path::new(&shown)) {
            self.drop_path(&shown);
            return;
        }
        match std::fs::read_to_string(&abs) {
            Ok(content) if !content.contains('\0') && content.len() <= MAX_FILE_BYTES => {
                let stamp = std::fs::metadata(&abs)
                    .map(|m| file_stamp(&m))
                    .unwrap_or((content.len() as i64, 0));
                self.upsert_file(&shown, &content, stamp);
            }
            _ => self.drop_path(&shown),
        }
    }

    fn drop_path(&self, path: &str) {
        let conn = crate::lock_unpoison(&self.conn);
        let _ = conn.execute("DELETE FROM chunks WHERE path = ?1", params![path]);
        let _ = conn.execute("DELETE FROM files WHERE path = ?1", params![path]);
    }

    fn upsert_file(&self, path: &str, content: &str, stamp: (i64, i64)) {
        let mut conn = crate::lock_unpoison(&self.conn);
        if let Ok(tx) = conn.transaction() {
            upsert_file_tx(&tx, path, content, stamp);
            let _ = tx.commit();
        };
    }

    pub(crate) fn search(&self, query: &str, path_filter: Option<&str>, limit: usize) -> Vec<Hit> {
        let cap = limit.clamp(1, HIT_CAP);
        let fetch = cap.saturating_mul(4).clamp(cap, 32);
        let mut out = Vec::new();
        let mut seen = HashSet::new();

        if is_glob(query) {
            if let Ok(hits) = self.search_glob(query, path_filter, fetch as i64) {
                merge_hits(&mut out, &mut seen, fetch, hits);
            }
            if !out.is_empty() {
                return out;
            }
        } else if looks_like_filename(query) {
            if let Ok(hits) = self.search_filename(query, path_filter, fetch as i64) {
                merge_hits(&mut out, &mut seen, fetch, hits);
            }
            if !out.is_empty() {
                rank_hits(&mut out, query, cap);
                return out;
            }
        }

        let idents = ident_tokens(query);
        let mut exact_idents = Vec::new();
        for ident in &idents {
            if let Ok(hits) = self.search_symbol(&ident, path_filter, fetch as i64) {
                if !hits.is_empty() {
                    exact_idents.push(ident.clone());
                }
                merge_hits(&mut out, &mut seen, fetch, hits);
            }
        }
        // An explicit identifier is a much stronger signal than surrounding
        // prose. Add reference chunks for that identifier, then converge;
        // never fill the result with unrelated matches for words like
        // "Windows", "bug", or "where".
        if !exact_idents.is_empty() {
            for ident in &exact_idents {
                let exact = format!("\"{}\"", ident.replace('"', ""));
                if let Ok(hits) = self.search_fts(&exact, path_filter, fetch as i64) {
                    merge_hits(&mut out, &mut seen, fetch, hits);
                }
            }
            rank_hits(&mut out, query, cap);
            return out;
        }

        let fts = fts_query(query);
        if !fts.is_empty() && out.len() < fetch {
            if let Ok(hits) = self.search_fts(&fts, path_filter, fetch as i64) {
                merge_hits(&mut out, &mut seen, fetch, hits);
            }
        }
        if out.len() < fetch {
            if let Ok(hits) = self.search_like(&search_tokens(query), path_filter, fetch as i64) {
                merge_hits(&mut out, &mut seen, fetch, hits);
            }
        }
        rank_hits(&mut out, query, cap);
        out
    }

    fn search_symbol(
        &self,
        ident: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let conn = crate::lock_unpoison(&self.conn);
        if let Some(p) = path_filter {
            let like = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE symbol = ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY path LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![ident, like, limit], row_to_hit)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE symbol = ?1 ORDER BY path LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![ident, limit], row_to_hit)?;
            rows.collect()
        }
    }

    fn search_filename(
        &self,
        name: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let needle = name.trim().trim_start_matches("./").replace('\\', "/");
        let like = format!("%{}", like_escape(&needle));
        let conn = crate::lock_unpoison(&self.conn);
        if let Some(p) = path_filter {
            let pf = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                   AND path LIKE ?3 ESCAPE '\\'
                 ORDER BY path, start LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![needle, like, pf, limit], row_to_hit)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'
                 ORDER BY path, start LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![needle, like, limit], row_to_hit)?;
            rows.collect()
        }
    }

    fn search_glob(
        &self,
        pattern: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let glob = pattern.replace("**/", "*").replace("**", "*");
        let conn = crate::lock_unpoison(&self.conn);
        let hits = if let Some(p) = path_filter {
            let like = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE path GLOB ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY path, start LIMIT ?3",
            )?;
            let mapped = stmt.query_map(params![glob, like, limit], row_to_hit)?;
            let collected = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
            collected
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE path GLOB ?1 ORDER BY path, start LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![glob, limit], row_to_hit)?;
            let collected = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
            collected
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for h in hits {
            if seen.insert(h.path.clone()) {
                out.push(h);
            }
        }
        Ok(out)
    }

    fn search_fts(
        &self,
        fts: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let conn = crate::lock_unpoison(&self.conn);
        if let Some(p) = path_filter {
            let like = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE chunks MATCH ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY rank LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![fts, like, limit], row_to_hit)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE chunks MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts, limit], row_to_hit)?;
            rows.collect()
        }
    }

    fn search_like(
        &self,
        tokens: &[String],
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let mut like_sql = String::new();
        for i in 0..tokens.len() {
            if i > 0 {
                like_sql.push_str(" OR ");
            }
            like_sql.push_str(&format!("body LIKE ?{} ESCAPE '\\'", i + 1));
        }
        let mut bind: Vec<rusqlite::types::Value> = tokens
            .iter()
            .map(|t| rusqlite::types::Value::Text(format!("%{}%", like_escape(t))))
            .collect();
        let sql = if let Some(p) = path_filter {
            bind.push(rusqlite::types::Value::Text(format!(
                "%{}%",
                like_escape(p)
            )));
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT path, start, end, body FROM chunks
                 WHERE ({like_sql}) AND path LIKE ?{} ESCAPE '\\' LIMIT ?{}",
                tokens.len() + 1,
                tokens.len() + 2
            )
        } else {
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT path, start, end, body FROM chunks WHERE {like_sql} LIMIT ?{}",
                tokens.len() + 1
            )
        };
        let conn = crate::lock_unpoison(&self.conn);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), row_to_hit)?;
        rows.collect()
    }
}

impl CodeIndex {
    pub fn render_query(&self, query: &str, path_filter: Option<&str>) -> Option<String> {
        let hits = self.search(query, path_filter, HIT_CAP);
        if hits.is_empty() {
            None
        } else {
            Some(render_hits(&hits, RENDER_CHARS))
        }
    }

    /// Cheap reverse lookup: other files whose chunks mention idents from `snippet`.
    /// Not LSP. Empty when nothing unique enough is found.
    pub fn referrer_hint(&self, edited_path: &str, snippet: &str) -> Option<String> {
        let edited = edited_path
            .trim()
            .trim_start_matches("./")
            .replace('\\', "/");
        if edited == ".grok-hyper"
            || edited.starts_with(".grok-hyper/")
            || edited.contains("/.grok-hyper/")
        {
            return None;
        }
        let mut idents = ident_tokens(snippet);
        if idents.is_empty() {
            idents = fts_tokens(snippet)
                .into_iter()
                .filter(|t| is_ident(t) || (t.len() >= 6 && t.contains('_')))
                .collect();
        }
        idents.sort();
        idents.dedup();
        let mut seen = HashSet::new();
        let mut locs: Vec<String> = Vec::new();
        for ident in idents.iter().take(8) {
            for h in self.search(ident, None, HIT_CAP) {
                let p = h.path.replace('\\', "/");
                if p == edited || p.ends_with(&format!("/{edited}")) {
                    continue;
                }
                let key = format!("{}:{}", p, h.start);
                if seen.insert(key.clone()) {
                    locs.push(key);
                }
                if locs.len() >= 5 {
                    break;
                }
            }
            if locs.len() >= 5 {
                break;
            }
        }
        if locs.is_empty() {
            return None;
        }
        Some(format!(
            "[refs] This change may affect {}; consider running related tests.",
            locs.join(", ")
        ))
    }
}

pub fn run_search(
    index: &CodeIndex,
    workspace: &Workspace,
    call: &ToolCall,
    limits: ToolLimits,
) -> ToolResponse {
    let query = arg_str(&call.arguments, "query").unwrap_or_default();
    if query.trim().is_empty() {
        return ToolResponse::text(&call.id, "Error: search needs `query`.", ToolState::Error);
    }
    let path = search_path_filter(
        workspace,
        arg_str(&call.arguments, "path")
            .filter(|s| !s.trim().is_empty())
            .as_deref(),
    );
    let mut hits = index.search(&query, path.as_deref(), HIT_CAP);
    let mut widened: Option<String> = None;
    if hits.is_empty() {
        if let Some(p) = path.as_deref() {
            let wide = index.search(&query, None, HIT_CAP);
            if !wide.is_empty() {
                widened = Some(p.to_string());
                hits = wide;
            }
        }
    }
    if hits.is_empty() {
        let hint = if index.is_empty() {
            SEARCH_WARMING
        } else {
            "No matches."
        };
        return ToolResponse::text(&call.id, hint, ToolState::Success);
    }
    let body = render_hits(&hits, RENDER_CHARS);
    let body = match widened {
        Some(p) => format!("Nothing under `{p}`. Workspace hits:\n{body}"),
        None => body,
    };
    folded_response(&call.id, body, ToolState::Success, limits, None)
}

/// Function-sized spans for a query. Empty if the index has no hits.
pub fn render_query_spans(index: &CodeIndex, query: &str) -> String {
    if query.trim().is_empty() {
        return String::new();
    }
    let hits = index.search(query, None, HIT_CAP);
    if hits.is_empty() {
        String::new()
    } else {
        render_hits(&hits, RENDER_CHARS)
    }
}

fn search_path_filter(ws: &Workspace, raw: Option<&str>) -> Option<String> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty())?;
    if matches!(raw, "." | "./" | "/") {
        return None;
    }
    let shown = ws.shown(raw).replace('\\', "/");
    let shown = shown.trim_start_matches("./");
    if shown.is_empty() || shown == "." {
        return None;
    }
    let root = ws.display().replace('\\', "/");
    let root = root.trim_end_matches('/');
    if shown == root {
        return None;
    }
    if let Some(rest) = shown.strip_prefix(&format!("{root}/")) {
        return if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        };
    }
    Some(shown.to_string())
}

fn merge_hits(out: &mut Vec<Hit>, seen: &mut HashSet<(String, u32)>, cap: usize, hits: Vec<Hit>) {
    for h in hits {
        if out.len() >= cap {
            break;
        }
        if seen.insert((h.path.clone(), h.start)) {
            out.push(h);
        }
    }
}

/// `src/foo/tests.rs` sorts before `src/foo/progress.rs` in SQLite. Production
/// spans should lead so a locate query does not open the unit-test first.
fn search_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/").to_lowercase();
    let file = p.rsplit('/').next().unwrap_or("");
    file == "tests.rs"
        || file == "tests.ts"
        || file == "tests.js"
        || file == "tests.py"
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_tests.rs")
        || p.split('/')
            .any(|s| s == "tests" || s == "test" || s == "__tests__")
}

fn prefer_production_hits(out: &mut Vec<Hit>) {
    let mut prod = Vec::new();
    let mut tests = Vec::new();
    for h in out.drain(..) {
        if search_test_path(&h.path) || test_chunk(&h.body) || schema_dump_chunk(&h.body) {
            tests.push(h);
        } else {
            prod.push(h);
        }
    }
    prod.extend(tests);
    *out = prod;
}

fn test_chunk(body: &str) -> bool {
    body.contains("#[test]") || body.contains("#[tokio::test]")
}

/// Frozen tool JSON in `tools_schema.rs` matches words like Shell/background
/// but is not the implementation.
fn schema_dump_chunk(body: &str) -> bool {
    body.contains("r#\"{\"type\":\"function\"")
        || (body.contains("\"type\":\"function\"") && body.contains("\"parameters\""))
}

/// Spans that actually contain a query identifier beat FTS prose matches.
fn prefer_ident_hits(out: &mut Vec<Hit>, query: &str) {
    let idents = ident_tokens(query);
    if idents.is_empty() {
        return;
    }
    let mut hit = Vec::new();
    let mut miss = Vec::new();
    for h in out.drain(..) {
        if idents
            .iter()
            .any(|id| h.body.contains(id.as_str()) || h.path.contains(id.as_str()))
        {
            hit.push(h);
        } else {
            miss.push(h);
        }
    }
    hit.extend(miss);
    *out = hit;
}

fn rank_hits(out: &mut Vec<Hit>, query: &str, cap: usize) {
    prefer_ident_hits(out, query);
    prefer_query_coverage(out, query);
    prefer_named_path(out, query);
    prefer_production_hits(out);
    out.truncate(cap);
}

/// NL leftover: the span that contains more query tokens (CJK phrase, not
/// the two-letter "IM" that matches every chat bridge comment).
fn prefer_query_coverage(out: &mut Vec<Hit>, query: &str) {
    let toks = search_tokens(query);
    if toks.len() < 2 {
        return;
    }
    out.sort_by(|a, b| {
        coverage(b, &toks)
            .cmp(&coverage(a, &toks))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn coverage(h: &Hit, toks: &[String]) -> usize {
    toks.iter()
        .filter(|t| h.body.contains(t.as_str()) || h.path.contains(t.as_str()))
        .count()
}

/// `Search("send_typing wechat")` should open `wechat.rs` before `qq.rs`.
fn prefer_named_path(out: &mut Vec<Hit>, query: &str) {
    let tokens: Vec<String> = fts_tokens(query)
        .into_iter()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| {
            t.len() >= 4 && !is_syntax_kw(t) && !is_stopword(t) && !is_generic_path_token(t)
        })
        .collect();
    if tokens.is_empty() {
        return;
    }
    let mut named = Vec::new();
    let mut rest = Vec::new();
    for h in out.drain(..) {
        let p = h.path.replace('\\', "/").to_ascii_lowercase();
        let file = p.rsplit('/').next().unwrap_or("");
        let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
        if tokens
            .iter()
            .any(|t| stem == t || file.contains(t) || p.split('/').any(|seg| seg == t))
        {
            named.push(h);
        } else {
            rest.push(h);
        }
    }
    if named.is_empty() {
        *out = rest;
        return;
    }
    named.extend(rest);
    *out = named;
}

fn is_generic_path_token(t: &str) -> bool {
    matches!(
        t,
        "src"
            | "lib"
            | "crates"
            | "channel"
            | "tools"
            | "agent"
            | "session"
            | "hyper"
            | "loop"
            | "test"
            | "tests"
            | "code"
            | "main"
            | "mod"
            | "index"
    )
}

fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    let start: i64 = row.get(1)?;
    let end: i64 = row.get(2)?;
    Ok(Hit {
        path: row.get(0)?,
        start: start.max(1) as u32,
        end: end.max(start).max(1) as u32,
        body: row.get(3)?,
    })
}

fn render_hits(hits: &[Hit], cap: usize) -> String {
    let mut out = String::new();
    let mut wrote_hit = false;
    for h in hits {
        let block = format_hit(h);
        if wrote_hit && out.chars().count() + block.chars().count() > cap {
            break;
        }
        if wrote_hit {
            out.push('\n');
        }
        out.push_str(&block);
        wrote_hit = true;
    }
    out
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 0 && version != INDEX_SCHEMA {
        conn.execute_batch("DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS chunks;")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files(
           path TEXT PRIMARY KEY,
           size INTEGER NOT NULL,
           mtime_ns INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
           path,
           start UNINDEXED,
           end UNINDEXED,
           symbol,
           body,
           tokenize = \"unicode61 tokenchars '_'\"
         );",
    )?;
    conn.pragma_update(None, "user_version", INDEX_SCHEMA)?;
    Ok(())
}

fn file_stamp(meta: &std::fs::Metadata) -> (i64, i64) {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    (meta.len().min(i64::MAX as u64) as i64, modified)
}

fn drop_path_tx(tx: &Transaction<'_>, path: &str) {
    let _ = tx.execute("DELETE FROM chunks WHERE path = ?1", params![path]);
    let _ = tx.execute("DELETE FROM files WHERE path = ?1", params![path]);
}

fn upsert_file_tx(tx: &Transaction<'_>, path: &str, content: &str, stamp: (i64, i64)) {
    let _ = tx.execute("DELETE FROM chunks WHERE path = ?1", params![path]);
    for ch in chunk_file(content) {
        let _ = tx.execute(
            "INSERT INTO chunks (path, start, end, symbol, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![path, ch.start as i64, ch.end as i64, ch.symbol, ch.body],
        );
    }
    let _ = tx.execute(
        "INSERT INTO files(path, size, mtime_ns) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime_ns=excluded.mtime_ns",
        params![path, stamp.0, stamp.1],
    );
}

fn format_hit(h: &Hit) -> String {
    let def = h.body.lines().next().is_some_and(is_def_line);
    let mut s = if def {
        format!("## [def] {}:{}-{}\n", h.path, h.start, h.end)
    } else {
        format!("## {}:{}-{}\n", h.path, h.start, h.end)
    };
    for (i, line) in h.body.lines().enumerate() {
        s.push_str(&format!("{:>6}|{}\n", h.start as usize + i, line));
    }
    s
}

#[derive(Debug)]
struct Chunk {
    start: u32,
    end: u32,
    symbol: String,
    body: String,
}

fn chunk_file(content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.is_empty() || (lines.len() == 1 && lines[0].is_empty()) {
        return Vec::new();
    }
    let n = lines.len();
    let mut bounds = vec![0usize];
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && is_def_line(line) {
            bounds.push(i);
        }
    }
    bounds.push(n);
    let mut starts: Vec<usize> = Vec::with_capacity(bounds.len());
    for (idx, &b) in bounds.iter().enumerate() {
        if idx == bounds.len() - 1 {
            starts.push(n);
            break;
        }
        let pulled = item_prefix_start(&lines, b);
        let floor = starts.last().copied().unwrap_or(0);
        starts.push(pulled.max(floor));
    }
    let mut out = Vec::new();
    for w in starts.windows(2) {
        let mut a = w[0];
        let b = w[1];
        while a < b {
            let end = (a + CHUNK_LINES).min(b);
            let slice = &lines[a..end];
            let body = slice.join("\n");
            if !body.trim().is_empty() {
                out.push(Chunk {
                    start: (a + 1) as u32,
                    end: end as u32,
                    symbol: slice
                        .iter()
                        .find(|l| is_def_line(l))
                        .map(|l| symbol_of(l))
                        .unwrap_or_default(),
                    body,
                });
            }
            a = end;
        }
    }
    out
}

/// Doc comments and attributes belong to the *next* item, not the previous
/// `const`/`fn` span. Otherwise Search hits a one-line `const STREAM_ROTATE`
/// and the model Opens the file just to read the comment above it.
fn item_prefix_start(lines: &[&str], def: usize) -> usize {
    let mut i = def;
    while i > 0 {
        let prev = lines[i - 1];
        if is_def_line(prev) {
            break;
        }
        let t = prev.trim_start();
        if t.starts_with("///")
            || t.starts_with("//!")
            || t.starts_with("#[")
            || t.starts_with("#!")
        {
            i -= 1;
            continue;
        }
        break;
    }
    i
}

fn is_def_line(line: &str) -> bool {
    let mut t = line.trim_start();
    if t.starts_with("//") || t.starts_with("/*") {
        return false;
    }
    if t.starts_with('#') && !t.starts_with("#define") && !t.starts_with("#!") {
        return false;
    }
    loop {
        if let Some(rest) = t.strip_prefix("pub(crate) ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("pub ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("export ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("async ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("unsafe ") {
            t = rest;
        } else {
            break;
        }
    }
    [
        "fn ",
        "fn(",
        "struct ",
        "enum ",
        "impl ",
        "impl<",
        "trait ",
        "mod ",
        "type ",
        "class ",
        "def ",
        "function ",
        "interface ",
        "const ",
        "static ",
        "macro_rules!",
    ]
    .iter()
    .any(|k| t.starts_with(k))
}

fn symbol_of(line: &str) -> String {
    let mut t = line.trim_start();
    loop {
        if let Some(rest) = t.strip_prefix("pub(crate) ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("pub ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("export ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("async ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("unsafe ") {
            t = rest;
        } else {
            break;
        }
    }
    for key in [
        "fn",
        "struct",
        "enum",
        "impl",
        "trait",
        "mod",
        "type",
        "class",
        "def",
        "function",
        "interface",
        "const",
        "static",
        "macro_rules!",
    ] {
        if let Some(rest) = t.strip_prefix(key) {
            t = rest.trim_start().trim_start_matches(['<', '(']);
            break;
        }
    }
    t.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn should_index(rel: &Path) -> bool {
    if skip_rel(rel) {
        return false;
    }
    let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".lock") || name.ends_with(".min.js") {
        return false;
    }
    if matches!(
        name,
        "package-lock.json" | "Cargo.lock" | "yarn.lock" | "pnpm-lock.yaml"
    ) {
        return false;
    }
    let ext = rel
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "rs" | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "go"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "java"
            | "kt"
            | "toml"
            | "md"
            | "json"
            | "yaml"
            | "yml"
            | "sh"
            | "bash"
            | "sql"
            | "html"
            | "css"
            | "txt"
    ) || matches!(name, "Makefile" | "Dockerfile" | "CMakeLists.txt")
}

fn skip_rel(rel: &Path) -> bool {
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.starts_with("eval/nightly/work/") {
        return true;
    }
    if hyper_overlay_skipped(&s) {
        return true;
    }
    s.split('/').any(|c| SKIP_DIR.contains(&c))
}

fn hyper_overlay_skipped(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(".grok-hyper/") else {
        return false;
    };
    let first = rest.split('/').next().unwrap_or("");
    HYPER_SKIP_DIR.contains(&first)
}

fn skip_index_dir(name: &str, parent_rel: &str) -> bool {
    if SKIP_DIR.contains(&name) {
        return true;
    }
    let under = parent_rel == ".grok-hyper" || parent_rel.starts_with(".grok-hyper/");
    under && HYPER_SKIP_DIR.contains(&name)
}

struct ScanBudget {
    started: Instant,
    limit: Duration,
}

impl ScanBudget {
    fn new(limit: Duration) -> Self {
        Self {
            started: Instant::now(),
            limit,
        }
    }

    #[cfg(test)]
    fn unlimited() -> Self {
        Self::new(Duration::from_secs(3600))
    }

    fn exceeded(&self) -> bool {
        self.started.elapsed() >= self.limit
    }
}

/// Home, Desktop/Documents/Downloads, or a drive root. Scanning these on
/// Windows is how the first hop froze for minutes.
fn skip_index_root(root: &Path) -> bool {
    is_volume_root(root) || is_user_home(root) || is_profile_dump(root)
}

fn is_volume_root(root: &Path) -> bool {
    let c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    !c.components().any(|c| matches!(c, Component::Normal(_)))
}

fn is_user_home(root: &Path) -> bool {
    let Some(home) = crate::config::user_home() else {
        return false;
    };
    same_dir(root, &home)
}

fn is_profile_dump(root: &Path) -> bool {
    let Some(home) = crate::config::user_home() else {
        return false;
    };
    const NAMES: &[&str] = &[
        "Desktop",
        "Documents",
        "Downloads",
        "Pictures",
        "Videos",
        "Music",
        "OneDrive",
        "Dropbox",
        "桌面",
        "文档",
        "文稿",
        "下载",
        "图片",
        "音乐",
        "视频",
    ];
    NAMES.iter().any(|n| same_dir(root, &home.join(n)))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b
}

fn git_toplevel(root: &Path) -> Option<PathBuf> {
    let out = git_at(root, &["rev-parse", "--show-toplevel"])?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

fn git_at(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    crate::proc_spawn::hide_window(&mut cmd);
    cmd.args(["-C"])
        .arg(root)
        .args(["-c", "safe.directory=*"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => {
                let mut buf = Vec::new();
                let _ = child.stdout.as_mut()?.read_to_end(&mut buf);
                return Some(buf);
            }
            Ok(Some(_)) => return None,
            Ok(None) if started.elapsed() > GIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

/// `git ls-files` relative to `root`. None when git is missing, the repo is the
/// user home (accidental `~/.git`), or the discovered toplevel lives outside
/// the workspace — those cases would hide nested clones.
fn git_ls_files(root: &Path) -> Option<Vec<PathBuf>> {
    let top = git_toplevel(root)?;
    if is_user_home(&top) {
        return None;
    }
    let top_c = std::fs::canonicalize(&top).unwrap_or(top);
    let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !root_c.starts_with(&top_c) && !top_c.starts_with(&root_c) {
        return None;
    }
    git_ls_files_raw(root)
}

fn git_ls_files_raw(root: &Path) -> Option<Vec<PathBuf>> {
    let stdout = git_at(root, &["ls-files", "-z", "-c", "-o", "--exclude-standard"])?;
    if stdout.is_empty() {
        return None;
    }
    Some(
        stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).as_ref()))
            .collect(),
    )
}

/// Directories with their own `.git` that `git ls-files` on the parent skips.
fn collect_nested_git_files(root: &Path, budget: &ScanBudget) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_nested_git(root, root, 0, &mut out, budget);
    out
}

/// `.grok-hyper/` is gitignored; overnight scripts still need Search hits.
fn collect_hyper_overlay_files(root: &Path, budget: &ScanBudget) -> Vec<PathBuf> {
    let overlay = root.join(".grok-hyper");
    if !overlay.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk_dir(root, &overlay, &mut out, budget);
    out
}

const NESTED_GIT_DEPTH: usize = 8;

fn walk_nested_git(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
    budget: &ScanBudget,
) {
    if budget.exceeded() || out.len() >= MAX_FILES || depth > NESTED_GIT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if budget.exceeded() || out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let parent_rel = dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if skip_index_dir(name_s.as_ref(), &parent_rel) {
            continue;
        }
        let path = entry.path();
        if super::path::is_reparse_or_symlink(&path) {
            continue;
        }
        if path.join(".git").exists() {
            if let Some(files) = git_ls_files_raw(&path) {
                for f in files {
                    let abs = path.join(&f);
                    if let Ok(rel) = abs.strip_prefix(root) {
                        out.push(rel.to_path_buf());
                    }
                }
            } else {
                let mut nested = Vec::new();
                walk_dir(&path, &path, &mut nested, budget);
                for f in nested {
                    if let Ok(rel) = path.join(f).strip_prefix(root) {
                        out.push(rel.to_path_buf());
                    }
                }
            }
            continue;
        }
        walk_nested_git(root, &path, depth + 1, out, budget);
    }
}

fn walk_fallback(root: &Path, budget: &ScanBudget) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out, budget);
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<PathBuf>, budget: &ScanBudget) {
    if budget.exceeded() || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if budget.exceeded() || out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            let parent_rel = dir
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if skip_index_dir(name_s.as_ref(), &parent_rel) {
                continue;
            }
            if super::path::is_reparse_or_symlink(&path) {
                continue;
            }
            walk_dir(root, &path, out, budget);
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

fn fts_query(raw: &str) -> String {
    let tokens = search_tokens(raw);
    if tokens.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .filter(|t| t.len() > 2)
        .collect();
    if quoted.is_empty() {
        return String::new();
    }
    // Identifiers: AND (precise, Instant Grep-like). NL leftover: OR.
    let sep = if tokens.iter().any(|t| is_ident(t)) {
        " "
    } else {
        " OR "
    };
    quoted.join(sep)
}

fn search_tokens(raw: &str) -> Vec<String> {
    if let Some(s) = signature_query_ident(raw) {
        return vec![s];
    }
    let all: Vec<String> = fts_tokens(raw)
        .into_iter()
        .filter(|t| keep_search_token(t))
        .collect();
    let idents: Vec<String> = all.iter().filter(|t| is_ident(t)).cloned().collect();
    if !idents.is_empty() {
        return idents;
    }
    all.into_iter()
        .filter(|t| !is_stopword(t) && !is_syntax_kw(t))
        .collect()
}

fn ident_tokens(raw: &str) -> Vec<String> {
    let mut tokens: Vec<String> = fts_tokens(raw)
        .into_iter()
        .filter(|t| is_ident(t))
        .collect();
    if let Some(s) = signature_query_ident(raw) {
        if !tokens.iter().any(|t| t == &s) {
            tokens.insert(0, s);
        }
    }
    tokens
}

/// `Search("pub(crate) async fn send")` should look up `send`, not AND/OR
/// visibility keywords. Reuses the same vis-strip as `symbol_of`.
fn signature_query_ident(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 200 || !signature_kind(t) {
        return None;
    }
    let s = symbol_of(t);
    if s.len() < 2 || is_syntax_kw(&s) {
        return None;
    }
    Some(s)
}

fn signature_kind(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    [
        "fn ",
        "fn(",
        "def ",
        "function ",
        "struct ",
        "enum ",
        "impl ",
        "trait ",
        "class ",
        "interface ",
        "const ",
        "static ",
        "mod ",
        "type ",
        "macro_rules",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn is_syntax_kw(t: &str) -> bool {
    matches!(
        t.to_ascii_lowercase().as_str(),
        "pub"
            | "crate"
            | "async"
            | "await"
            | "fn"
            | "def"
            | "function"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "mod"
            | "type"
            | "class"
            | "const"
            | "static"
            | "export"
            | "unsafe"
            | "interface"
            | "let"
            | "mut"
            | "use"
            | "return"
    )
}

fn is_ident(t: &str) -> bool {
    if is_syntax_kw(t) {
        return false;
    }
    t.contains('_')
        || t.contains('/')
        || (t.chars().any(|c| c.is_ascii_uppercase())
            && t.chars().any(|c| c.is_ascii_lowercase())
            && t.len() >= 3)
}

fn is_stopword(t: &str) -> bool {
    matches!(
        t.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "the"
            | "is"
            | "are"
            | "was"
            | "be"
            | "do"
            | "does"
            | "how"
            | "where"
            | "what"
            | "which"
            | "who"
            | "we"
            | "to"
            | "of"
            | "in"
            | "on"
            | "for"
            | "and"
            | "or"
            | "find"
            | "search"
            | "code"
            | "file"
            | "function"
            | "please"
            | "show"
            | "me"
    ) || matches!(t, "在哪" | "哪里" | "怎么" | "如何" | "什么" | "查找")
}

fn is_glob(q: &str) -> bool {
    q.contains('*') || q.contains('?')
}

fn looks_like_filename(q: &str) -> bool {
    let t = q.trim();
    if t.is_empty() || t.contains(' ') {
        return false;
    }
    t.contains('/') || (t.contains('.') && !t.starts_with('.'))
}

/// Pattern the model asked grep/rg to find. None if this is not a repo search.
pub fn bash_search_query(command: &str) -> Option<String> {
    let tokens = shell_words(command);
    if tokens.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
        i += 1;
    }
    if i >= tokens.len() {
        return None;
    }
    let cmd = tokens[i].as_str();
    let rest_i = if cmd == "git" && tokens.get(i + 1).map(|s| s.as_str()) == Some("grep") {
        i + 2
    } else if matches!(cmd, "rg" | "ripgrep" | "ag" | "ack") {
        i + 1
    } else if cmd == "grep" {
        let recursive = tokens.iter().skip(i + 1).any(|t| {
            matches!(t.as_str(), "-r" | "-R" | "-n" | "-H" | "--recursive")
                || t.starts_with("-n")
                || t.starts_with("-r")
                || looks_like_filename(t)
        });
        if !recursive {
            return None;
        }
        i + 1
    } else {
        return None;
    };
    grep_pattern(&tokens[rest_i..])
}

pub fn search_dump_too_big(text: &str) -> bool {
    text.chars().count() > 1500 || text.lines().count() > 20
}

fn grep_pattern(tokens: &[String]) -> Option<String> {
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if t == "--" {
            return tokens.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        if matches!(t, "-e" | "-F" | "-E" | "--regexp" | "--fixed-strings") {
            return tokens.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        if let Some(rest) = t.strip_prefix("-e") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        if !t.is_empty() {
            return Some(t.to_string());
        }
        i += 1;
    }
    None
}

fn shell_words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>') => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if !c.is_whitespace() {
                    break;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn fts_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut cur_ascii = true;
    for c in raw.chars() {
        let ascii = c.is_ascii_alphanumeric() || c == '_';
        let cjk = is_cjk(c);
        if !ascii && !cjk {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() && cur_ascii != ascii {
            tokens.push(std::mem::take(&mut cur));
        }
        cur_ascii = ascii;
        cur.push(c);
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn keep_search_token(t: &str) -> bool {
    let n = t.chars().count();
    if t.chars().any(is_cjk) {
        return n >= 2;
    }
    n >= 3
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4e00}'..='\u{9fff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{3000}'..='\u{303f}'
            | '\u{3040}'..='\u{30ff}'
            | '\u{ff00}'..='\u{ffef}'
    )
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use std::process::Command;

    fn scratch() -> (std::path::PathBuf, Workspace) {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/policy.rs"),
            "pub struct ThinkPolicy {\n    pub max_think_tokens: u32,\n}\n\n\
             fn ignored() {}\n\n\
             fn upgrade_medium(&mut self) {\n    if self.user_locked { return; }\n    self.policy.max_think_tokens = 2048;\n}\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        (dir, ws)
    }

    #[test]
    fn chunks_split_on_fn() {
        let src = "fn a() {}\n\nfn b() {}\n";
        let ch = chunk_file(src);
        assert!(ch.len() >= 2, "{ch:?}");
        assert_eq!(ch[0].symbol, "a");
        assert_eq!(ch[1].symbol, "b");
    }

    #[test]
    fn chunk_keeps_doc_comment_with_following_item() {
        let src = concat!(
            "const MAX: usize = 1;\n",
            "/// rotate early so a long turn keeps a think bubble\n",
            "const STREAM_ROTATE: usize = 2;\n",
        );
        let ch = chunk_file(src);
        let rot = ch
            .iter()
            .find(|c| c.symbol == "STREAM_ROTATE")
            .expect("STREAM_ROTATE chunk");
        assert!(
            rot.body.contains("rotate early"),
            "doc should ride with the next item: {}",
            rot.body
        );
        assert!(rot.body.contains("STREAM_ROTATE"));
        let max = ch.iter().find(|c| c.symbol == "MAX").expect("MAX chunk");
        assert!(
            !max.body.contains("rotate early"),
            "previous const must not steal the next item's docs: {}",
            max.body
        );
    }

    #[test]
    fn search_returns_span_not_whole_file() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium", None, 8);
        assert!(!hits.is_empty(), "expected a hit");
        let h = &hits[0];
        assert!(h.path.contains("policy.rs"), "{}", h.path);
        assert!(h.body.contains("fn upgrade_medium"));
        assert!(
            !h.body.contains("struct ThinkPolicy"),
            "preamble leaked: {}",
            h.body
        );
        let call = ToolCall {
            id: "t".into(),
            name: "search".into(),
            arguments: json!({"query": "upgrade_medium"}),
        };
        let out = run_search(&idx, &ws, &call, ToolLimits::default());
        let text = out.joined_text();
        assert!(text.contains("## "), "{text}");
        assert!(text.contains("|"), "{text}");
        assert!(text.contains("upgrade_medium"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_ranks_production_ahead_of_tests_rs() {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/tests.rs"),
            "fn zh_think_keep() { /* unit test double */ }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/progress.rs"),
            "fn zh_think_keep(line: &str) -> bool { line.chars().any(|c| c as u32 > 127) }\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("zh_think_keep", None, 8);
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(
            hits[0].path.contains("progress.rs"),
            "production span should lead, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_ranks_ident_span_ahead_of_unrelated_prose() {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/memory.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn hot_card_skips_local_questions() {\n        let timeout = 1;\n        assert_eq!(timeout, 1);\n    }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/timeout.rs"),
            "pub struct Deadlines { pub kill_at: Option<u64> }\nfn arm_kill_deadline() { let kill_at = 1; }\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("kill_at Shell background timeout", None, 8);
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(
            hits[0].path.contains("timeout.rs"),
            "kill_at span should lead, got {:?}",
            hits.iter()
                .map(|h| (&h.path, h.body.chars().take(40).collect::<String>()))
                .collect::<Vec<_>>()
        );
        assert!(hits[0].body.contains("kill_at"), "{}", hits[0].body);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_ranks_impl_ahead_of_tool_schema_json() {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/schema.rs"),
            concat!(
                "const SHELL: &str = r#\"",
                r#"{"type":"function","function":{"name":"Shell","description":"background timeout","parameters":{}}}"#,
                "\"#;\n",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("src/timeout.rs"),
            "fn arm_kill_deadline() { let kill_at = 1; let background = true; }\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("shell background timeout", None, 8);
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(
            hits[0].path.contains("timeout.rs"),
            "impl span should lead schema JSON, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_index_search_does_not_steer_to_grep() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::empty();
        let call = ToolCall {
            id: "t".into(),
            name: "search".into(),
            arguments: json!({"query": "nothing-here"}),
        };
        let out = run_search(&idx, &ws, &call, ToolLimits::default());
        let text = out.joined_text();
        assert!(text.contains("No matches"), "{text}");
        assert!(text.contains("warming"), "{text}");
        assert!(!text.contains("Glob"), "{text}");
        assert!(!text.contains("Grep"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workspace_root_path_does_not_hide_hits() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let call = ToolCall {
            id: "t".into(),
            name: "search".into(),
            arguments: json!({
                "query": "upgrade_medium",
                "path": ws.root().display().to_string(),
            }),
        };
        let text = run_search(&idx, &ws, &call, ToolLimits::default()).joined_text();
        assert!(text.contains("upgrade_medium"), "{text}");
        assert_eq!(search_path_filter(&ws, Some(".")), None);
        assert_eq!(
            search_path_filter(&ws, Some(ws.root().to_str().unwrap())),
            None
        );
        assert_eq!(search_path_filter(&ws, Some("src")), Some("src".into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refresh_picks_up_new_fn() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        assert!(idx.search("note_thrash", None, 8).is_empty());
        std::fs::write(
            dir.join("src/policy.rs"),
            std::fs::read_to_string(dir.join("src/policy.rs")).unwrap()
                + "\nfn note_thrash(&mut self) { self.upgrade_medium(); }\n",
        )
        .unwrap();
        idx.refresh(&ws, "src/policy.rs");
        let hits = idx.search("note_thrash", None, 8);
        assert!(hits.iter().any(|h| h.body.contains("note_thrash")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_backed_sync_reuses_unchanged_chunks() {
        let (dir, ws) = scratch();
        let db = dir.join("index.sqlite3");
        let conn = Connection::open(&db).unwrap();
        init_schema(&conn).unwrap();
        let idx = CodeIndex {
            conn: Mutex::new(conn),
        };
        idx.sync_root(
            ws.root(),
            walk_fallback(ws.root(), &ScanBudget::unlimited()),
            &ScanBudget::unlimited(),
        );
        let before: Vec<i64> = {
            let conn = crate::lock_unpoison(&idx.conn);
            let mut stmt = conn
                .prepare("SELECT rowid FROM chunks ORDER BY rowid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        idx.sync_root(
            ws.root(),
            walk_fallback(ws.root(), &ScanBudget::unlimited()),
            &ScanBudget::unlimited(),
        );
        let after: Vec<i64> = {
            let conn = crate::lock_unpoison(&idx.conn);
            let mut stmt = conn
                .prepare("SELECT rowid FROM chunks ORDER BY rowid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(before, after, "unchanged files should not be re-indexed");
        drop(idx);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn path_filter_narrows() {
        let (dir, ws) = scratch();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("other/x.rs"), "fn upgrade_medium() {}\n").unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium", Some("src/"), 8);
        assert!(hits.iter().all(|h| h.path.starts_with("src/")), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_path_filter_widens_to_workspace() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let miss = ToolCall {
            id: "t".into(),
            name: "search".into(),
            arguments: json!({
                "query": "upgrade_medium",
                "path": "crates/hyper-cli/src/channels.rs",
            }),
        };
        let out = run_search(&idx, &ws, &miss, ToolLimits::default());
        let text = out.joined_text();
        assert!(
            text.contains("Nothing under `crates/hyper-cli/src/channels.rs`"),
            "{text}"
        );
        assert!(text.contains("upgrade_medium"), "{text}");
        assert!(!text.starts_with("No matches"), "{text}");
        let hit = ToolCall {
            id: "t2".into(),
            name: "search".into(),
            arguments: json!({
                "query": "upgrade_medium",
                "path": "src",
            }),
        };
        let scoped = run_search(&idx, &ws, &hit, ToolLimits::default());
        let scoped_text = scoped.joined_text();
        assert!(
            !scoped_text.contains("Nothing under"),
            "hits in the named path must stay scoped: {scoped_text}"
        );
        assert!(scoped_text.contains("upgrade_medium"), "{scoped_text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nl_query_strips_stopwords() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("where is the think cap", None, 8);
        assert!(
            hits.iter()
                .any(|h| h.body.contains("max_think_tokens") || h.body.contains("ThinkPolicy")),
            "{hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn signature_query_strips_vis_and_fn() {
        assert_eq!(
            ident_tokens("pub(crate) async fn send"),
            vec!["send".to_string()]
        );
        assert_eq!(
            search_tokens("pub(crate) async fn send"),
            vec!["send".to_string()]
        );
        assert_eq!(
            ident_tokens("pub async fn send_progress"),
            vec!["send_progress".to_string()]
        );
        assert!(ident_tokens("where is the think cap").is_empty());
        assert!(
            !search_tokens("中文 IM 滤英文思考")
                .iter()
                .any(|t| t.eq_ignore_ascii_case("im")),
            "two-letter IM must not OR-match every chat comment: {:?}",
            search_tokens("中文 IM 滤英文思考")
        );
    }

    #[test]
    fn cjk_nl_query_ranks_keep_fn_over_im_card() {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/sticky.rs"),
            "pub const IM_CARD_ZH: &str = \"[im] 即时消息。思考过程和回复都必须用中文。不要用英文写思考。\";\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/progress.rs"),
            "/// 中文 IM 滤英文思考：只保留 CJK 占优的行和片段\nfn zh_think_keep(s: &str) -> String { s.into() }\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("中文 IM 滤英文思考", None, 8);
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(
            hits[0].path.contains("progress.rs"),
            "CJK locate must open the filter fn, not the IM card: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits[0].body.contains("zh_think_keep"), "{:?}", hits[0].body);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn signature_query_finds_pub_async_fn_not_vis_noise() {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/noise.rs"),
            "pub(crate) async fn other() { let crate_async = 1; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/wecom.rs"),
            "pub async fn send(text: &str) { let _ = text; }\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("pub(crate) async fn send", None, 8);
        assert!(
            hits.iter()
                .any(|h| h.path.contains("wecom.rs") && h.body.contains("fn send")),
            "pub async fn send should match pub(crate) async fn send: {hits:?}"
        );
        assert!(
            hits.iter().all(|h| h.body.contains("send")),
            "visibility keywords must not rank crate/async noise: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn named_path_in_query_ranks_that_file_first() {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/qq.rs"),
            "pub async fn send_typing(chat: &str) { let _ = chat; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/wechat.rs"),
            "pub async fn send_typing(chat: &str) { let _ = chat; }\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("send_typing wechat", None, 8);
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(
            hits[0].path.contains("wechat.rs"),
            "named path should lead: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filename_query_finds_path() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("policy.rs", None, 8);
        assert!(
            hits.iter().any(|h| h.path.ends_with("policy.rs")),
            "{hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_indexes_overnight_overlay() {
        let (dir, ws) = scratch();
        let overnight = dir.join(".grok-hyper/overnight");
        std::fs::create_dir_all(&overnight).unwrap();
        std::fs::write(
            overnight.join("hops.py"),
            "A hop is an `assistant` event whose `tool_calls` list is non-empty.\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join(".grok-hyper/sessions")).unwrap();
        std::fs::write(dir.join(".grok-hyper/sessions/sid.py"), "tool_calls = []\n").unwrap();
        let idx = CodeIndex::build(ws.root());
        let by_name = idx.search("hops.py", None, 8);
        assert!(
            by_name.iter().any(|h| h
                .path
                .replace('\\', "/")
                .ends_with(".grok-hyper/overnight/hops.py")),
            "filename Search must see overlay: {by_name:?}"
        );
        let by_body = idx.search("tool_calls", None, 8);
        assert!(
            by_body
                .iter()
                .any(|h| h.path.replace('\\', "/").contains("overnight/hops.py")),
            "body Search must see overlay: {by_body:?}"
        );
        assert!(
            by_body
                .iter()
                .all(|h| !h.path.replace('\\', "/").contains("sessions/")),
            "session dumps must stay out: {by_body:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_indexes_gitignored_overnight_in_git_workspace() {
        let (dir, ws) = scratch();
        let overnight = dir.join(".grok-hyper/overnight");
        std::fs::create_dir_all(&overnight).unwrap();
        std::fs::write(
            overnight.join("hops.py"),
            "def load_events(path):\n    pass\n",
        )
        .unwrap();
        git_init(ws.root());
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("hops.py", None, 8);
        assert!(
            hits.iter().any(|h| h
                .path
                .replace('\\', "/")
                .contains(".grok-hyper/overnight/hops.py")),
            "git ls-files must not hide overlay scripts: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_lists_matching_files() {
        let (dir, ws) = scratch();
        std::fs::write(dir.join("src/other.py"), "def unused():\n    pass\n").unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("src/*.rs", None, 8);
        assert!(hits.iter().any(|h| h.path == "src/policy.rs"), "{hits:?}");
        assert!(hits.iter().all(|h| h.path.ends_with(".rs")), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn symbol_hit_ranks_before_body_mentions() {
        let (dir, ws) = scratch();
        std::fs::write(
            dir.join("src/call.rs"),
            "fn other() {\n    upgrade_medium();\n}\n",
        )
        .unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium", None, 8);
        assert!(!hits.is_empty());
        assert!(
            hits[0].body.contains("fn upgrade_medium"),
            "definition should lead: {}",
            hits[0].body
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn referrer_hint_lists_other_files() {
        let (dir, ws) = scratch();
        std::fs::write(
            dir.join("src/call.rs"),
            "fn other() {\n    upgrade_medium();\n}\n",
        )
        .unwrap();
        let idx = CodeIndex::build(ws.root());
        let hint = idx
            .referrer_hint("src/policy.rs", "fn upgrade_medium(&mut self) {")
            .expect("hint");
        assert!(hint.contains("src/call.rs"), "{hint}");
        assert!(!hint.contains("src/policy.rs:"), "{hint}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn referrer_hint_skips_hyper_overlay() {
        let (dir, ws) = scratch();
        std::fs::write(
            dir.join("src/call.rs"),
            "fn other() {\n    upgrade_medium();\n}\n",
        )
        .unwrap();
        let idx = CodeIndex::build(ws.root());
        assert!(idx
            .referrer_hint(
                ".grok-hyper/overnight/score_all.py",
                "fn upgrade_medium(&mut self) {"
            )
            .is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exact_identifier_does_not_fill_with_background_words() {
        let (dir, ws) = scratch();
        std::fs::write(
            dir.join("README.md"),
            "Windows PATH setup and general troubleshooting notes.\n",
        )
        .unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium on Windows PATH", None, 8);
        assert!(!hits.is_empty());
        assert!(
            hits.iter().all(|h| h.body.contains("upgrade_medium")),
            "background prose leaked into exact-symbol results: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_search_query_extracts_rg_and_grep() {
        assert_eq!(
            bash_search_query("rg upgrade_medium").as_deref(),
            Some("upgrade_medium")
        );
        assert_eq!(
            bash_search_query("grep -n drive loop.rs").as_deref(),
            Some("drive")
        );
        assert_eq!(
            bash_search_query("git grep -n 'ThinkPolicy'").as_deref(),
            Some("ThinkPolicy")
        );
        assert!(bash_search_query("ps aux | grep foo").is_none());
        assert!(bash_search_query("python3 -m unittest").is_none());
        assert!(search_dump_too_big(&"x\n".repeat(25)));
        assert!(!search_dump_too_big("ok\n"));
    }

    #[test]
    fn skip_rel_drops_windows_profile_junk() {
        assert!(skip_rel(Path::new("AppData/Local/foo.rs")));
        assert!(skip_rel(Path::new("Library/Caches/bar.py")));
        assert!(skip_rel(Path::new("OneDrive/docs.rs")));
        assert!(skip_rel(Path::new("Downloads/setup.rs")));
        assert!(!skip_rel(Path::new("src/foo.rs")));
        assert!(!skip_rel(Path::new(".grok-hyper/overnight/hops.py")));
        assert!(skip_rel(Path::new(".grok-hyper/sessions/sid.jsonl")));
        assert!(skip_rel(Path::new(".grok-hyper/blobs/ab")));
    }

    #[test]
    fn skip_index_root_covers_home_and_volume() {
        assert!(is_volume_root(Path::new("/")));
        if let Some(home) = crate::config::user_home() {
            assert!(skip_index_root(&home));
        }
    }

    fn git_init(dir: &Path) {
        let st = Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git init");
        assert!(
            st.status.success(),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    #[test]
    fn search_indexes_nested_git_repos() {
        let dir =
            std::env::temp_dir().join(format!("hyper-nest-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("nested/src")).unwrap();
        git_init(&dir);
        std::fs::write(dir.join("README.md"), "outer workspace\n").unwrap();
        git_init(&dir.join("nested"));
        std::fs::write(
            dir.join("nested/src/hit.rs"),
            "fn nested_search_needle() {}\n",
        )
        .unwrap();
        let idx = CodeIndex::build(&dir);
        let hits = idx.search("nested_search_needle", None, 8);
        assert!(
            hits.iter()
                .any(|h| h.path.contains("hit.rs") && h.body.contains("nested_search_needle")),
            "nested repo was invisible to search: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn git_ls_files_ignores_home_repo() {
        let Some(home) = crate::config::user_home() else {
            return;
        };
        assert!(is_user_home(&home));
        // Even if HOME is a git repo, list_index must not treat it as the project.
        // git_ls_files returns None so the caller walks / finds nested clones.
        if git_toplevel(&home).is_some_and(|top| same_dir(&top, &home)) {
            assert!(git_ls_files(&home).is_none());
        }
    }
}
