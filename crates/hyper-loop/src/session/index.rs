//! Write-through FTS5 over session JSONL. Source of truth remains the JSONL.
//!
//! Thinking/`reasoning` is not indexed (27B think is noisy and was never
//! evidence). Recall-tool names are reserved so a later search cannot match
//! its own previous queries.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::session::event::SessionEvent;

const SKIP_TOOL_NAMES: &[&str] = &[
    "recall",
    "recall_history",
    "memory_search",
    "search",
    "Grep",
    "grep",
    "skill",
    "mcp",
    "view",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub session_id: String,
    pub seq: i64,
    pub kind: String,
    pub name: Option<String>,
    pub blob: Option<String>,
    pub snippet: String,
}

pub struct HistoryIndex {
    conn: Mutex<Connection>,
}

impl HistoryIndex {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("history.sqlite");
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .map_err(Error::msg)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE VIRTUAL TABLE IF NOT EXISTS turns USING fts5(
               session_id UNINDEXED,
               seq UNINDEXED,
               kind UNINDEXED,
               name UNINDEXED,
               blob UNINDEXED,
               body,
               tokenize = 'unicode61'
             );",
        )
        .map_err(Error::msg)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert(&self, session_id: &str, seq: i64, event: &SessionEvent) -> Result<()> {
        let Some((kind, name, blob, body)) = index_body(event) else {
            return Ok(());
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("history index poisoned"))?;
        conn.execute(
            "DELETE FROM turns WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq],
        )
        .map_err(Error::msg)?;
        conn.execute(
            "INSERT INTO turns (session_id, seq, kind, name, blob, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, seq, kind, name, blob, body],
        )
        .map_err(Error::msg)?;
        Ok(())
    }

    pub fn reindex_session(&self, session_id: &str, events: &[SessionEvent]) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("history index poisoned"))?;
        conn.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(Error::msg)?;
        drop(conn);
        for (seq, event) in events.iter().enumerate() {
            self.upsert(session_id, seq as i64, event)?;
        }
        Ok(())
    }

    pub fn search(&self, query: &str, session_id: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
        let limit = limit.clamp(1, 50) as i64;
        let tokens = fts_tokens(query);
        let fts = fts_query(query);
        if !fts.is_empty() {
            match self.search_fts(&fts, session_id, limit) {
                Ok(hits) if !hits.is_empty() => return Ok(hits),
                _ => {}
            }
        }
        self.search_like(&tokens, session_id, limit)
    }

    fn search_fts(
        &self,
        fts: &str,
        session_id: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let conn = self.conn.lock().expect("history index poisoned");
        let sql = if session_id.is_some() {
            "SELECT session_id, seq, kind, name, blob,
                    snippet(turns, 5, '[', ']', '…', 12)
             FROM turns WHERE turns MATCH ?1 AND session_id = ?2
             ORDER BY rank LIMIT ?3"
        } else {
            "SELECT session_id, seq, kind, name, blob,
                    snippet(turns, 5, '[', ']', '…', 12)
             FROM turns WHERE turns MATCH ?1
             ORDER BY rank LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(sid) = session_id {
            stmt.query_map(params![fts, sid, limit], row_to_hit)?
        } else {
            stmt.query_map(params![fts, limit], row_to_hit)?
        };
        rows.collect()
    }

    fn search_like(
        &self,
        tokens: &[String],
        session_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Hit>> {
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
        let sql = if let Some(sid) = session_id {
            bind.push(rusqlite::types::Value::Text(sid.to_string()));
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT session_id, seq, kind, name, blob, substr(body, 1, 160)
             FROM turns WHERE ({like_sql}) AND session_id = ?{}
             LIMIT ?{}",
                tokens.len() + 1,
                tokens.len() + 2
            )
        } else {
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT session_id, seq, kind, name, blob, substr(body, 1, 160)
             FROM turns WHERE {like_sql} LIMIT ?{}",
                tokens.len() + 1
            )
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("history index poisoned"))?;
        let mut stmt = conn.prepare(&sql).map_err(Error::msg)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind), row_to_hit)
            .map_err(Error::msg)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::msg)
    }
}

fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    Ok(Hit {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        kind: row.get(2)?,
        name: row.get(3)?,
        blob: row.get(4)?,
        snippet: row.get(5)?,
    })
}

fn index_body(
    event: &SessionEvent,
) -> Option<(&'static str, Option<String>, Option<String>, String)> {
    match event {
        SessionEvent::User(u)
            if !u.text.trim().is_empty() && !crate::template::is_hidden_user_text(&u.text) =>
        {
            Some(("user", None, None, u.text.clone()))
        }
        SessionEvent::Assistant(a) => {
            let mut body = a.content.clone();
            if let Some(calls) = &a.tool_calls {
                for c in calls {
                    if SKIP_TOOL_NAMES.contains(&c.function.name.as_str()) {
                        continue;
                    }
                    body.push('\n');
                    body.push_str(&c.function.name);
                    body.push(' ');
                    body.push_str(&c.function.arguments);
                }
            }
            if body.trim().is_empty() {
                return None;
            }
            Some(("assistant", None, None, body))
        }
        SessionEvent::Tool(t) => {
            if SKIP_TOOL_NAMES.contains(&t.name.as_str()) {
                return None;
            }
            let mut body = t.name.clone();
            body.push('\n');
            body.push_str(&t.output);
            Some(("tool", Some(t.name.clone()), t.blob.clone(), body))
        }
        _ => None,
    }
}

/// Bounded lexical fallback over the source of truth. Also serves current-chat
/// retrieval without opening/rebuilding SQLite on every follow-up. Chinese
/// phrases use overlapping bigrams: unicode61 treats an unspaced sentence as
/// one token, so ordinary MATCH cannot recall a differently phrased question.
pub(crate) fn search_events(
    session_id: &str,
    events: &[SessionEvent],
    query: &str,
    limit: usize,
    spoken_only: bool,
) -> Vec<Hit> {
    let query: String = query.chars().take(512).collect();
    let mut terms = Vec::new();
    for token in fts_tokens(&query.to_lowercase()) {
        if token.chars().any(is_cjk) {
            let chars: Vec<char> = token.chars().collect();
            if chars.len() > 2 {
                for pair in chars.windows(2) {
                    terms.push(pair.iter().collect::<String>());
                }
            }
        }
        if token.len() > 1
            && !["the", "what", "did", "we", "about", "please", "remember"]
                .contains(&token.as_str())
        {
            terms.push(token);
        }
    }
    terms.sort();
    terms.dedup();
    terms.truncate(64);
    let undos: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Undo(u) => Some((u.from_seq, u.until_seq)),
            _ => None,
        })
        .collect();
    let mut scored = Vec::new();
    for (seq, event) in events.iter().enumerate() {
        if undos
            .iter()
            .any(|(from, until)| seq as u64 >= *from && seq as u64 <= *until)
        {
            continue;
        }
        if spoken_only
            && !matches!(
                event,
                SessionEvent::User(_)
                    | SessionEvent::Assistant(crate::session::event::AssistantEvent {
                        tool_calls: None,
                        ..
                    })
            )
        {
            continue;
        }
        let Some((kind, name, blob, body)) = index_body(event) else {
            continue;
        };
        let lower = body.to_lowercase();
        let matches: Vec<_> = terms
            .iter()
            .filter(|t| lower.contains(t.as_str()))
            .collect();
        let score: usize = matches.iter().map(|t| t.chars().count().min(12)).sum();
        if score == 0 {
            continue;
        }
        // Return evidence around the match, not always the first 160 chars.
        // Use character positions to avoid slicing through UTF-8 boundaries.
        let needle = matches.iter().max_by_key(|t| t.chars().count()).unwrap();
        let pos = lower.find(needle.as_str()).unwrap_or(0);
        let start = lower[..pos].chars().count().saturating_sub(80);
        let snippet = body.chars().skip(start).take(1200).collect();
        scored.push((
            score,
            seq,
            Hit {
                session_id: session_id.into(),
                seq: seq as i64,
                kind: kind.into(),
                name,
                blob,
                snippet,
            },
        ));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    scored
        .into_iter()
        .take(limit.clamp(1, 50))
        .map(|(_, _, h)| h)
        .collect()
}

fn fts_query(raw: &str) -> String {
    fts_tokens(raw)
        .into_iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .filter(|t| t.len() > 2)
        .collect::<Vec<_>>()
        .join(" ")
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

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2CEAF}'
            | '\u{30000}'..='\u{3134F}'
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
    use crate::session::event::SessionEvent;

    #[test]
    fn source_recall_matches_rephrased_chinese_and_ignores_hidden_cards() {
        let events = vec![
            SessionEvent::user("租户隔离必须通过 tenant_id 实现，禁止全局共享缓存"),
            SessionEvent::assistant("已经记录这个约束", "hidden-secret", None),
            SessionEvent::user(crate::template::wrap_tool_response(
                "租户隔离 hidden-card-secret",
            )),
            SessionEvent::tool("r", "recall", "租户隔离 recursive-search-noise"),
        ];
        let hits = search_events("s", &events, "前面租户隔离是怎么约定的？", 4, true);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].seq, 0);
        assert!(hits[0].snippet.contains("tenant_id"));
        assert!(search_events("s", &events, "hidden-secret", 4, false).is_empty());
    }

    #[test]
    fn source_recall_centers_long_event_on_matching_evidence() {
        let events = vec![SessionEvent::user(format!(
            "{} migration_key=opal-731 {}",
            "padding ".repeat(300),
            "tail ".repeat(300)
        ))];
        let hits = search_events("s", &events, "migration_key", 4, true);
        assert!(hits[0].snippet.contains("opal-731"));
        assert!(hits[0].snippet.chars().count() <= 1200);
    }

    fn tmp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hyper-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn indexes_user_and_tool_not_reasoning() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::user("fix the prefix cache miss"))
            .unwrap();
        idx.upsert(
            "s",
            2,
            &SessionEvent::assistant("ok", "I will secretly mention zirconium", None),
        )
        .unwrap();
        idx.upsert(
            "s",
            3,
            &SessionEvent::tool("c1", "bash", "cargo test passed"),
        )
        .unwrap();

        let hits = idx.search("prefix cache", Some("s"), 10).unwrap();
        assert!(hits.iter().any(|h| h.kind == "user"), "{hits:?}");

        let secret = idx.search("zirconium", Some("s"), 10).unwrap();
        assert!(
            secret.is_empty(),
            "reasoning must not be searchable: {secret:?}"
        );

        let cargo = idx.search("cargo", Some("s"), 10).unwrap();
        assert!(cargo.iter().any(|h| h.kind == "tool"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_recall_tool_rows() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert(
            "s",
            1,
            &SessionEvent::tool("c9", "recall", "unique-needle-xyz"),
        )
        .unwrap();
        assert!(idx
            .search("unique-needle-xyz", Some("s"), 10)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_mcp_and_skill_rows() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::tool("c9", "mcp", "secret-token-xyz"))
            .unwrap();
        idx.upsert(
            "s",
            2,
            &SessionEvent::tool("c8", "skill", "unique-skill-body-xyz"),
        )
        .unwrap();
        assert!(idx
            .search("secret-token-xyz", Some("s"), 10)
            .unwrap()
            .is_empty());
        assert!(idx
            .search("unique-skill-body-xyz", Some("s"), 10)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fts_query_splits_hyphen_keeps_cjk() {
        let q = fts_query("grok-hyper audit 源码修改");
        assert!(!q.contains("qharness"), "{q}");
        assert!(q.contains("\"grok\""), "{q}");
        assert!(q.contains("\"hyper\""), "{q}");
        assert!(q.contains("\"audit\""), "{q}");
        assert!(q.contains("\"源码修改\""), "{q}");
        assert!(fts_query("源码").contains("源码"));
        assert!(fts_query("源码修改").contains("源码修改"));
    }

    #[test]
    fn hyphenated_and_cjk_query_hits() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert(
            "s",
            1,
            &SessionEvent::user("compact note: grok-hyper audit 源码 and related work"),
        )
        .unwrap();
        let hits = idx
            .search("grok-hyper audit 源码修改", Some("s"), 10)
            .unwrap();
        assert!(hits.iter().any(|h| h.kind == "user"), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hyphen_query_does_not_glue_tokens() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::user("clone grok-hyper then probe"))
            .unwrap();
        let q = fts_query("grok-hyper");
        assert!(!q.contains("qharness"), "{q}");
        assert!(q.contains("\"grok\""), "{q}");
        assert!(q.contains("\"hyper\""), "{q}");
        let hits = idx.search("grok-hyper", Some("s"), 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pure_cjk_query_hits() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::user("昨日完成源码修改"))
            .unwrap();
        let hits = idx.search("源码", Some("s"), 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
