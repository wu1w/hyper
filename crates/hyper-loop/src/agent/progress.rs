//! Result-novelty after tools run. Permuted inspection loops are not
//! consecutive identical fingerprints, so DoomLoopGate never sees them.
//!
//! Hyper does not decide "is the task done?". It decides whether another
//! inspect hop would add evidence. If not, keep the frozen Cursor `tools[]`
//! mounted and nudge toward native Write / StrReplace / Task.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use sha2::{Digest, Sha256};

use crate::paw_loop::fs_tool_path;
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::Workspace;
use crate::tools_schema::dispatch_name;

use super::dispatch::canon_ws_path;

pub const HOP_HISTORY: usize = 8;
pub const LOW_STREAK: u32 = 3;
/// Consecutive inspect-only hops (Read/Grep/Glob/…) before a write-nudge.
/// Writes reset this. High enough that a real edit can still read a handful
/// of files first; low enough that an audit cannot wander the tree.
pub const INSPECT_STREAK: u32 = 10;
pub const NOVELTY_FLOOR: f32 = 0.15;

pub const STOP_NO_PROGRESS_SYNTHESIZED: &str = "no_progress_synthesized";
pub const STOP_NO_PROGRESS_EMPTY: &str = "no_progress_empty";

pub const FORCED_SYNTHESIS_NOTE: &str = "\
[trajectory] Further inspection is not adding enough new evidence. \
Do not call tools. Answer the user now using the evidence already collected. \
State remaining uncertainty explicitly.";

/// Trajectory nudge: keep the frozen Cursor `tools[]` mounted. Do not treat
/// this as tools=None. The next hop must be native Write / StrReplace / Task,
/// or a finished answer with no tools.
pub const WRITE_NOW_NOTE: &str = "\
[trajectory] Further inspection is not adding enough new evidence. \
Do not Read, Grep, or Glob again. Emit native Write / StrReplace / Task \
tool calls now — not JSON, HTML fences, or narration. \
If the work is done, answer without tools.";

pub const ALREADY_OBSERVED_MSG: &str = "\
[already observed]\nThis exact content was returned earlier this turn.\n\
No new evidence was added.\nUse the existing result or answer now.";

/// Skip result for inspect-only hops after the write-nudge. Cursor keeps
/// `tools[]` mounted; the model still sees a paired tool result.
pub const INSPECT_SKIP_MSG: &str = "\
[already observed]\nInspection is not adding enough new evidence this turn.\n\
Do not Read, Grep, or Glob again. Call Write, StrReplace, or Task, or answer now.";

#[derive(Clone, Debug, Default)]
pub struct ProgressDelta {
    pub new_paths: usize,
    pub new_evidence_hashes: usize,
    pub changed_files: usize,
    pub changed_test_state: bool,
    pub changed_diagnostics: bool,
    pub repeated_results: usize,
    pub total_results: usize,
}

impl ProgressDelta {
    pub fn novelty(&self) -> f32 {
        self.new_evidence_hashes as f32 / self.total_results.max(1) as f32
    }

    pub fn is_low(&self) -> bool {
        self.novelty() < NOVELTY_FLOOR
            && self.new_paths == 0
            && self.changed_files == 0
            && !self.changed_test_state
            && !self.changed_diagnostics
    }
}

#[derive(Clone, Debug, Default)]
struct HopSignature {
    tool_keys: BTreeSet<String>,
    result_hashes: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct ProgressTracker {
    seen_hashes: HashSet<String>,
    seen_at: HashMap<String, u32>,
    seen_paths: HashSet<String>,
    seen_matches: HashSet<String>,
    hops: VecDeque<HopSignature>,
    last_fps: BTreeSet<String>,
    low_streak: u32,
    inspect_streak: u32,
    hop_index: u32,
    last_test_fail: Option<bool>,
    last_diag_hash: Option<String>,
    synthesize: bool,
}

impl ProgressTracker {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn should_synthesize(&self) -> bool {
        self.synthesize
    }

    /// A recovered Write / Task is progress. Do not keep the inspect-skip hold
    /// for the rest of the turn after the model finally mutates the workspace.
    pub fn clear_synthesis(&mut self) {
        self.synthesize = false;
        self.inspect_streak = 0;
        self.low_streak = 0;
    }

    /// Fold repeated blobs, count novelty, then decide whether the next hop
    /// should skip further inspection (tools stay mounted).
    pub fn fold_and_observe(
        &mut self,
        ws: &Workspace,
        calls: &[ToolCall],
        responses: &mut [ToolResponse],
        test_red: bool,
        saw_test_output: bool,
        diag: Option<&str>,
    ) -> ProgressDelta {
        self.hop_index = self.hop_index.saturating_add(1);
        let fps = hop_fingerprints(calls);
        let mut delta = ProgressDelta::default();
        let mut sig = HopSignature::default();

        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            let name = dispatch_name(&call.name);
            sig.tool_keys.insert(tool_key(ws, call));
            if response.state != ToolState::Success {
                let h = evidence_hash(&canonicalize_output(&response.joined_text()));
                self.remember_hash(&h);
                delta.total_results += 1;
                continue;
            }
            if matches!(name, "write" | "edit" | "delete" | "editnotebook") {
                delta.changed_files += 1;
                if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                    let key = canon_ws_path(ws, &path);
                    if self.seen_paths.insert(key.clone()) {
                        delta.new_paths += 1;
                    }
                    let id = format!("{name}:{key}");
                    self.count_evidence(&id, &mut delta);
                    sig.result_hashes.insert(id);
                }
                continue;
            }

            let body = response.joined_text();
            let canon = canonicalize_output(&body);
            let blob_hash = evidence_hash(&canon);
            // Capture before count_evidence / remember_hash, or the first
            // dump of a blob is folded away as "already observed".
            let already = self.seen_hashes.contains(&blob_hash);
            if is_gate_or_nudge(&body) {
                delta.total_results += 1;
                delta.repeated_results += 1;
                self.remember_hash(&blob_hash);
                continue;
            }
            let listing = matches!(name, "glob" | "grep" | "search");
            if listing {
                self.note_listing(&canon, &mut delta, response);
            } else if matches!(name, "read" | "view") {
                if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                    let key = canon_ws_path(ws, &path);
                    if self.seen_paths.insert(key.clone()) {
                        delta.new_paths += 1;
                    }
                    let id = format!("read:{key}");
                    self.count_evidence(&id, &mut delta);
                    sig.result_hashes.insert(id);
                } else {
                    self.count_evidence(&blob_hash, &mut delta);
                    sig.result_hashes.insert(blob_hash.clone());
                }
            } else {
                self.count_evidence(&blob_hash, &mut delta);
                sig.result_hashes.insert(blob_hash.clone());
            }

            if already && self.should_fold_blob(name, &body) {
                let hop = self.seen_at.get(&blob_hash).copied().unwrap_or(0);
                *response = ToolResponse::text(
                    response.id.clone(),
                    already_observed_text(hop),
                    ToolState::Success,
                );
            }
            self.remember_hash(&blob_hash);
        }

        if saw_test_output || test_red {
            let fail = test_red;
            if self.last_test_fail != Some(fail) {
                delta.changed_test_state = true;
                self.last_test_fail = Some(fail);
            }
        }
        if let Some(diag) = diag {
            let h = evidence_hash(&canonicalize_output(diag));
            if self.last_diag_hash.as_deref() != Some(h.as_str()) {
                delta.changed_diagnostics = true;
                self.last_diag_hash = Some(h);
            }
        }

        let identical = !fps.is_empty() && fps == self.last_fps;
        self.last_fps = fps;
        if delta.is_low() && !identical {
            self.low_streak = self.low_streak.saturating_add(1);
        } else if !identical {
            self.low_streak = 0;
        }
        let inspect = hop_is_inspect(calls);
        if inspect && !identical {
            self.inspect_streak = self.inspect_streak.saturating_add(1);
        } else if !inspect {
            self.inspect_streak = 0;
            self.low_streak = 0;
            self.synthesize = false;
        }
        if self.low_streak >= LOW_STREAK || self.inspect_streak >= INSPECT_STREAK {
            self.synthesize = true;
        }
        self.hops.push_back(sig);
        while self.hops.len() > HOP_HISTORY {
            self.hops.pop_front();
        }
        delta
    }

    fn count_evidence(&mut self, id: &str, delta: &mut ProgressDelta) {
        delta.total_results += 1;
        if self.seen_hashes.contains(id) {
            delta.repeated_results += 1;
        } else {
            delta.new_evidence_hashes += 1;
            self.remember_hash(id);
        }
    }

    fn remember_hash(&mut self, h: &str) {
        if self.seen_hashes.insert(h.to_string()) {
            self.seen_at.insert(h.to_string(), self.hop_index);
        }
    }

    fn should_fold_blob(&self, name: &str, body: &str) -> bool {
        if !matches!(name, "read" | "view" | "grep" | "glob" | "search" | "bash") {
            return false;
        }
        if body.chars().count() < 80 {
            return false;
        }
        if is_gate_or_nudge(body) {
            return false;
        }
        true
    }

    fn note_listing(
        &mut self,
        canon: &str,
        delta: &mut ProgressDelta,
        response: &mut ToolResponse,
    ) {
        let mut new_matches = 0usize;
        let mut old_matches = 0usize;
        let mut ids: Vec<String> = Vec::new();
        for line in canon.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with("No files matching") || t.starts_with("No matches") {
                continue;
            }
            let id = evidence_hash(t);
            ids.push(id.clone());
            if self.seen_matches.insert(id) {
                new_matches += 1;
            } else {
                old_matches += 1;
            }
        }
        if ids.is_empty() {
            let h = evidence_hash(canon);
            self.count_evidence(&h, delta);
        } else {
            for id in &ids {
                self.count_evidence(id, delta);
            }
        }
        if old_matches > 0 {
            let extra =
                format!("\nnew_matches: {new_matches}\npreviously_seen_matches: {old_matches}");
            response
                .content
                .push(crate::tool_calls::TextBlock { text: extra });
        }
    }
}

fn is_gate_or_nudge(body: &str) -> bool {
    body.contains("[already observed]")
        || body.contains("[trajectory]")
        || body.contains("Already Read")
        || body.contains("Already searched")
        || body.contains("Already located")
        || body.contains("Grep budget")
        || body.contains("Search budget")
        || body.contains("Do not Glob")
        || body.contains("Do not Read sibling")
        || body.contains("The user already named")
        || body.contains("The user forbade")
        || body.contains("Search already dumped")
        || body.contains("Search already located")
        || body.contains("Do not Shell cat")
        || body.contains("Do not call Search")
        || body.contains("Already Grep'd")
        || body.contains("similar pattern this turn")
}

pub fn is_mutating_dispatch(name: &str) -> bool {
    matches!(
        dispatch_name(name),
        "write"
            | "edit"
            | "delete"
            | "editnotebook"
            | "generateimage"
            | "ask"
            | "switchmode"
            | "task"
            | "computeruse"
            | "calldynamictool"
    )
}

/// Inspect tools skipped after write-nudge. Matches `WRITE_NOW_NOTE`.
/// Shell / TodoWrite / Web* are work, not a re-read.
pub fn is_held_inspect(name: &str) -> bool {
    matches!(
        dispatch_name(name),
        "read" | "grep" | "glob" | "search" | "view"
    )
}

fn hop_is_inspect(calls: &[ToolCall]) -> bool {
    !calls.is_empty() && calls.iter().all(|c| !is_mutating_dispatch(&c.name))
}

fn already_observed_text(hop: u32) -> String {
    if hop == 0 {
        ALREADY_OBSERVED_MSG.to_string()
    } else {
        format!(
            "[already observed]\nThis exact content was returned at hop {hop}.\n\
No new evidence was added.\nUse the existing result or answer now."
        )
    }
}

fn hop_fingerprints(calls: &[ToolCall]) -> BTreeSet<String> {
    calls
        .iter()
        .map(|c| format!("{}:{}", dispatch_name(&c.name), c.arguments))
        .collect()
}

fn tool_key(ws: &Workspace, call: &ToolCall) -> String {
    let d = dispatch_name(&call.name);
    match d {
        "read" | "view" | "write" | "edit" | "delete" | "editnotebook" => {
            let p = fs_tool_path(&call.name, &call.arguments)
                .map(|p| canon_ws_path(ws, &p))
                .unwrap_or_default();
            format!("{d}:{p}")
        }
        "glob" => {
            let pat = crate::tools::arg_str(&call.arguments, "glob_pattern")
                .or_else(|| crate::tools::arg_str(&call.arguments, "pattern"))
                .unwrap_or_default();
            let dir =
                crate::tools::arg_str(&call.arguments, "target_directory").unwrap_or_default();
            format!("glob:{dir}:{pat}")
        }
        "grep" => {
            let pat = crate::tools::arg_str(&call.arguments, "pattern").unwrap_or_default();
            let path = crate::tools::arg_str(&call.arguments, "path").unwrap_or_default();
            format!("grep:{path}:{pat}")
        }
        "search" => {
            let q = crate::tools::arg_str(&call.arguments, "query").unwrap_or_default();
            format!("search:{q}")
        }
        "bash" => {
            let cmd = crate::tools::arg_str(&call.arguments, "command").unwrap_or_default();
            let clip: String = cmd.chars().take(80).collect();
            format!("bash:{clip}")
        }
        _ => d.to_string(),
    }
}

pub fn canonicalize_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = strip_line_number_prefix(line);
        let line = strip_truncation_line(line);
        if line.is_empty() {
            continue;
        }
        let line = strip_timestamps(line);
        if line.is_empty() {
            continue;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn strip_line_number_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    let digits = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > digits && i < bytes.len() && bytes[i] == b'|' {
        return &line[i + 1..];
    }
    line
}

fn strip_truncation_line(line: &str) -> &str {
    let t = line.trim();
    if (t.starts_with('…') || t.starts_with("..."))
        && (t.contains("truncated") || t.contains("scan stopped") || t.contains("omitted"))
    {
        return "";
    }
    line
}

fn strip_timestamps(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if let Some(n) = iso_or_clock_len(&b[i..]) {
            out.push_str("<ts>");
            i += n;
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn iso_or_clock_len(b: &[u8]) -> Option<usize> {
    // 2026-08-31T16:10:00 or 16:10:00
    if b.len() >= 19
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
    {
        return Some(19);
    }
    if b.len() >= 8
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2] == b':'
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
        && b[5] == b':'
        && b[6].is_ascii_digit()
        && b[7].is_ascii_digit()
    {
        return Some(8);
    }
    None
}

fn evidence_hash(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    format!("{d:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_strips_line_numbers_and_truncation() {
        let raw = "   12|fn ping()\n… truncated at 200 paths. Narrow the glob.\n16:10:00 done\n";
        let c = canonicalize_output(raw);
        assert!(c.contains("fn ping()"), "{c}");
        assert!(!c.contains("truncated"), "{c}");
        assert!(c.contains("<ts>"), "{c}");
    }

    #[test]
    fn low_progress_needs_no_new_paths_or_writes() {
        let mut d = ProgressDelta {
            new_evidence_hashes: 1,
            total_results: 10,
            ..ProgressDelta::default()
        };
        assert!(d.is_low());
        d.new_paths = 1;
        assert!(!d.is_low());
        d.new_paths = 0;
        d.changed_files = 1;
        assert!(!d.is_low());
    }

    #[test]
    fn identical_fingerprint_hops_do_not_count_toward_synthesis() {
        // DoomLoopGate owns exact repeats. Three identical Reads must not
        // strip tools before the sixth-call halt.
        let mut t = ProgressTracker::default();
        let low = ProgressDelta {
            total_results: 1,
            repeated_results: 1,
            ..ProgressDelta::default()
        };
        assert!(low.is_low());
        t.last_fps.insert("read:{}".into());
        t.low_streak = 0;
        let fps = t.last_fps.clone();
        let identical = fps == t.last_fps;
        assert!(identical);
        if low.is_low() && !identical {
            t.low_streak += 1;
        }
        assert_eq!(t.low_streak, 0);
    }

    #[test]
    fn write_clears_forced_synthesis() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-prog-write-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn ping() {}\n").unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let mut t = ProgressTracker::default();
        t.inspect_streak = INSPECT_STREAK;
        t.synthesize = true;
        assert!(t.should_synthesize());
        let write = ToolCall {
            id: "w".into(),
            name: "Write".into(),
            arguments: serde_json::json!({"path": "a.rs", "contents": "fn ping() { 1 }\n"}),
        };
        let mut wresp = [ToolResponse::text("w", "wrote a.rs", ToolState::Success)];
        t.fold_and_observe(&ws, &[write], &mut wresp, false, false, None);
        assert!(!t.should_synthesize());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn held_inspect_is_read_family_not_shell() {
        assert!(is_held_inspect("Read"));
        assert!(is_held_inspect("Grep"));
        assert!(is_held_inspect("Glob"));
        assert!(is_held_inspect("Search"));
        assert!(!is_held_inspect("Shell"));
        assert!(!is_held_inspect("bash"));
        assert!(!is_held_inspect("TodoWrite"));
        assert!(!is_held_inspect("WebSearch"));
        assert!(!is_held_inspect("Write"));
    }
}
