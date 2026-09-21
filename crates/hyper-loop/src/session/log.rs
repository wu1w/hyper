use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::policy::ThinkPolicy;
use crate::session::event::{SessionEvent, SessionStart};
use crate::session::index::{HistoryIndex, Hit};
use crate::session::{derive_messages, live_policy};
use crate::template::ChatMessage;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Append-only JSONL at `~/.grok-hyper/sessions/<id>.jsonl` (mode 0600, exclusive flock on write).
pub struct SessionLog {
    dir: PathBuf,
    id: String,
    path: PathBuf,
    events: Vec<SessionEvent>,
    index: Option<HistoryIndex>,
    bytes: u64,
}

impl SessionLog {
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Config::home_dir()?.join("sessions"))
    }

    pub fn create(start: SessionStart) -> Result<Self> {
        Self::create_in(Self::sessions_dir()?, start)
    }

    pub fn open(id: &str) -> Result<Self> {
        Self::open_in(Self::sessions_dir()?, id)
    }

    pub fn create_in(dir: impl AsRef<Path>, start: SessionStart) -> Result<Self> {
        let dir = dir.as_ref();
        ensure_dir(dir)?;
        let path = jsonl_path(dir, &start.id);
        if path.exists() {
            return Err(Error::msg(format!(
                "session already exists: {}",
                path.display()
            )));
        }
        let mut log = Self {
            dir: dir.to_path_buf(),
            id: start.id.clone(),
            path,
            events: Vec::new(),
            index: HistoryIndex::open(dir).ok(),
            bytes: 0,
        };
        log.write_event(SessionEvent::Start(start))?;
        Ok(log)
    }

    pub fn open_in(dir: impl AsRef<Path>, id: impl AsRef<str>) -> Result<Self> {
        let dir = dir.as_ref();
        let id = id.as_ref().to_string();
        let path = jsonl_path(dir, &id);
        let mut events = read_jsonl(&path)?;
        if !matches!(events.first(), Some(SessionEvent::Start(_))) {
            return Err(Error::msg(format!(
                "session JSONL missing session/start as events[0]: {}",
                path.display()
            )));
        }
        stub_archived_tools(&mut events);
        stub_excess_live_tools(&mut events);
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir: dir.to_path_buf(),
            id,
            path,
            events,
            index: HistoryIndex::open(dir).ok(),
            bytes,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Archived tool dumps are stubbed in RAM after compact. JSONL still has
    /// the original line; recall uses this so seq expansion is not the stub.
    pub fn event_at(&self, seq: usize) -> Option<SessionEvent> {
        let ram = self.events.get(seq)?;
        if !is_archived_tool_stub(ram) {
            return Some(ram.clone());
        }
        read_jsonl_nth(&self.path, seq).ok()
    }

    pub fn start(&self) -> Option<&SessionStart> {
        match self.events.first() {
            Some(SessionEvent::Start(s)) => Some(s),
            _ => None,
        }
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        derive_messages(&self.events)
    }

    pub fn policy(&self) -> Option<ThinkPolicy> {
        live_policy(&self.events)
    }

    pub fn append(&mut self, event: SessionEvent) -> Result<()> {
        if event.is_ephemeral() {
            return Ok(());
        }
        if matches!(event, SessionEvent::Start(_)) {
            return Err(Error::msg(
                "session/start must be events[0]; append a policy event instead of a second start",
            ));
        }
        self.write_event(event)
    }

    /// Rewrite `events[0].workspace`. Resume / `bind_store` read this field;
    /// in-memory sidecar changes alone do not survive restart.
    pub fn set_workspace(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let ws = path.as_ref().display().to_string();
        match self.events.first_mut() {
            Some(SessionEvent::Start(s)) => {
                if s.workspace == ws {
                    return Ok(());
                }
                s.workspace = ws.clone();
            }
            _ => return Err(Error::msg("missing session/start")),
        }
        // Rewrite from disk so RAM stubs after compact cannot erase JSONL.
        let mut disk = read_jsonl(&self.path)?;
        match disk.first_mut() {
            Some(SessionEvent::Start(s)) => s.workspace = ws,
            _ => return Err(Error::msg("missing session/start")),
        }
        rewrite_jsonl(&self.path, &disk)?;
        self.bytes = fs::metadata(&self.path)
            .map(|m| m.len())
            .unwrap_or(self.bytes);
        if let Some(index) = &self.index {
            let _ = index.reindex_session(&self.id, &disk);
        }
        Ok(())
    }

    /// Copy this JSONL to `start.id`, replace events[0], then append `session/fork`.
    /// Depth/`policy` events stay in the new file. `/think` `/fast` must not call this.
    pub fn fork(&self, start: SessionStart) -> Result<Self> {
        if start.id == self.id {
            return Err(Error::msg("fork requires a new session id"));
        }
        if self.events.is_empty() || !matches!(self.events[0], SessionEvent::Start(_)) {
            return Err(Error::msg("cannot fork: missing session/start"));
        }
        ensure_dir(&self.dir)?;
        let path = jsonl_path(&self.dir, &start.id);
        if path.exists() {
            return Err(Error::msg(format!(
                "session already exists: {}",
                path.display()
            )));
        }
        fs::copy(&self.path, &path)?;
        set_file_mode(&path)?;
        let mut log = Self::open_in(&self.dir, &start.id)?;
        match log.events.first_mut() {
            Some(SessionEvent::Start(_)) => {
                log.events[0] = SessionEvent::Start(start.clone());
            }
            _ => return Err(Error::msg("cannot fork: missing session/start")),
        }
        let mut disk = read_jsonl(&path)?;
        disk[0] = SessionEvent::Start(start);
        rewrite_jsonl(&path, &disk)?;
        log.bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if let Some(index) = &log.index {
            let _ = index.reindex_session(&log.id, &disk);
        }
        log.append(SessionEvent::fork(self.id.clone()))?;
        Ok(log)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        if query.is_ascii() {
            if let Some(index) = &self.index {
                if let Ok(hits) = index.search(query, Some(&self.id), limit) {
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(crate::session::index::search_events(
            &self.id,
            &self.events,
            query,
            limit,
            false,
        ))
    }

    fn write_event(&mut self, event: SessionEvent) -> Result<()> {
        let line = encode_line(&event)?;
        let add = line.len() as u64;
        if self.bytes.saturating_add(add) > MAX_SESSION_BYTES {
            return Err(Error::msg(format!(
                "session is too large to append ({} bytes; max {} bytes)",
                self.bytes.saturating_add(add),
                MAX_SESSION_BYTES
            )));
        }
        append_jsonl(&self.path, &event)?;
        self.bytes = self.bytes.saturating_add(add);
        let compacting = matches!(event, SessionEvent::Compact(_));
        self.events.push(event);
        if let Some(index) = &self.index {
            let seq = (self.events.len() - 1) as i64;
            let _ = index.upsert(&self.id, seq, self.events.last().unwrap());
        }
        if compacting {
            stub_archived_tools(&mut self.events);
        }
        stub_excess_live_tools(&mut self.events);
        Ok(())
    }

    fn official_path(&self) -> PathBuf {
        self.dir.join(format!("{}.official.json", self.id))
    }

    pub fn save_official(
        &self,
        item: &crate::session::OfficialCompaction,
        skip: usize,
    ) -> Result<()> {
        let path = self.official_path();
        if crate::tools::is_special_file(&path) {
            return Err(Error::msg(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let body = serde_json::to_vec(&item.persist_skip(skip)).map_err(Error::msg)?;
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let mut f = opts.open(&path)?;
        #[cfg(unix)]
        {
            let _ = f.lock_exclusive();
        }
        f.write_all(&body)?;
        f.write_all(b"\n")?;
        Ok(())
    }

    pub fn load_official(&self) -> Option<(crate::session::OfficialCompaction, usize)> {
        let path = self.official_path();
        if crate::tools::is_special_file(&path) {
            return None;
        }
        let meta = fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > OFFICIAL_MAX_BYTES {
            return None;
        }
        let raw = crate::tools::read_bytes_capped(&path, OFFICIAL_MAX_BYTES).ok()?;
        let raw = String::from_utf8(raw).ok()?;
        let p: crate::session::OfficialPersist = serde_json::from_str(&raw).ok()?;
        if p.encrypted_content.is_empty() {
            return None;
        }
        let skip = p.skip;
        Some((crate::session::OfficialCompaction::from_persist(p), skip))
    }

    pub fn clear_official(&self) {
        let _ = fs::remove_file(self.official_path());
    }
}

fn jsonl_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn encode_line(event: &SessionEvent) -> Result<String> {
    let mut line = serde_json::to_string(event).map_err(Error::msg)?;
    line.push('\n');
    Ok(line)
}

fn ensure_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn set_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn open_rw_create(path: &Path) -> Result<File> {
    if crate::tools::is_special_file(path) {
        return Err(Error::msg(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NONBLOCK);
    }
    Ok(opts.open(path)?)
}

fn append_jsonl(path: &Path, event: &SessionEvent) -> Result<()> {
    let mut file = open_rw_create(path)?;
    set_file_mode(path)?;
    file.lock_exclusive()?;
    let result = (|| {
        file.write_all(encode_line(event)?.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    let _ = file.unlock();
    result
}

fn rewrite_jsonl(path: &Path, events: &[SessionEvent]) -> Result<()> {
    if crate::tools::is_special_file(path) {
        return Err(Error::msg(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut opts = OpenOptions::new();
    opts.write(true).truncate(true).create(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = opts.open(path)?;
    set_file_mode(path)?;
    file.lock_exclusive()?;
    let result = write_events_locked(&mut file, events);
    let _ = file.unlock();
    result
}

fn write_events_locked(file: &mut File, events: &[SessionEvent]) -> Result<()> {
    for event in events {
        file.write_all(encode_line(event)?.as_bytes())?;
    }
    file.sync_all()?;
    Ok(())
}

/// Folded tool dumps stay on disk. After compact, RAM only keeps a stub so an
/// overnight session cannot hold tens of MiB of already-archived output.
const ARCHIVED_TOOL_KEEP: usize = 256;
/// Even before compact, keep only this much unstubbed tool output in RAM
/// (newest first). JSONL / recall(seq) still have the original.
const LIVE_TOOL_RAM: usize = 4 * 1024 * 1024;

fn latest_compact_until(events: &[SessionEvent]) -> Option<usize> {
    events.iter().rev().find_map(|e| match e {
        SessionEvent::Compact(c) => Some(c.until_seq as usize),
        _ => None,
    })
}

fn is_archived_tool_stub(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Tool(t) if t.output.starts_with("[archived seq=")
    )
}

fn stub_tool_at(i: usize, event: &mut SessionEvent) {
    let SessionEvent::Tool(t) = event else {
        return;
    };
    if t.output.len() <= ARCHIVED_TOOL_KEEP || t.output.starts_with("[archived seq=") {
        return;
    }
    t.output = if let Some(sha) = &t.blob {
        format!("[archived seq={i}; recall(blob={sha})]")
    } else {
        format!("[archived seq={i}; use recall(seq)]")
    };
    t.media.clear();
}

fn stub_archived_tools(events: &mut [SessionEvent]) {
    let Some(until) = latest_compact_until(events) else {
        return;
    };
    for (i, event) in events.iter_mut().enumerate() {
        if i == 0 || i > until {
            continue;
        }
        stub_tool_at(i, event);
    }
}

/// Cap live (post-compact) tool dumps in RAM so working_window=0 overnight
/// cannot grow to the 64MiB JSONL ceiling.
fn stub_excess_live_tools(events: &mut [SessionEvent]) {
    let skip_until = latest_compact_until(events).unwrap_or(0);
    let mut kept = 0usize;
    let mut keep_from = events.len();
    for i in (1..events.len()).rev() {
        if i <= skip_until {
            break;
        }
        let n = match &events[i] {
            SessionEvent::Tool(t)
                if t.output.len() > ARCHIVED_TOOL_KEEP
                    && !t.output.starts_with("[archived seq=") =>
            {
                t.output.len()
            }
            _ => continue,
        };
        if kept > 0 && kept.saturating_add(n) > LIVE_TOOL_RAM {
            keep_from = i + 1;
            for (j, event) in events.iter_mut().enumerate() {
                if j == 0 || j <= skip_until || j >= keep_from {
                    continue;
                }
                stub_tool_at(j, event);
            }
            return;
        }
        kept = kept.saturating_add(n);
    }
}

fn read_jsonl_nth(path: &Path, seq: usize) -> Result<SessionEvent> {
    if crate::tools::is_special_file(path) {
        return Err(Error::msg(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let file = crate::tools::open_read_nonblock(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut idx = 0usize;
    loop {
        line.clear();
        let n = read_record(&mut reader, &mut line)?;
        if n == 0 {
            return Err(Error::msg(format!("seq {seq} out of range")));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if idx == seq {
            return serde_json::from_slice(&line).map_err(Error::msg);
        }
        idx += 1;
    }
}

fn read_jsonl(path: &Path) -> Result<Vec<SessionEvent>> {
    if crate::tools::is_special_file(path) {
        return Err(Error::msg(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let meta = fs::metadata(path)?;
    if !meta.is_file() {
        return Err(Error::msg(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_SESSION_BYTES {
        return Err(Error::msg(format!(
            "{} is too large to load ({} bytes; max {} bytes)",
            path.display(),
            meta.len(),
            MAX_SESSION_BYTES
        )));
    }
    // Recovery must hold the same exclusive lock as append, so another writer
    // cannot complete the tail while we diagnose/truncate it.
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NONBLOCK);
    let mut file = opts.open(path)?;
    file.lock_exclusive()?;
    let result = (|| {
        let mut events = Vec::new();
        let mut reader = BufReader::new(&file);
        let mut line = Vec::new();
        let mut offset = 0u64;
        let mut loaded = 0u64;
        let mut repair = None;
        let mut needs_newline = false;
        loop {
            line.clear();
            let n = read_record(&mut reader, &mut line)?;
            if n == 0 {
                break;
            }
            loaded = loaded.saturating_add(n as u64);
            if loaded > MAX_SESSION_BYTES {
                return Err(Error::msg(format!(
                    "{} is too large to load (max {} bytes)",
                    path.display(),
                    MAX_SESSION_BYTES
                )));
            }
            let terminated = line.last() == Some(&b'\n');
            if line.iter().all(u8::is_ascii_whitespace) {
                offset += n as u64;
                continue;
            }
            match serde_json::from_slice::<SessionEvent>(&line) {
                Ok(event) => {
                    events.push(event);
                    needs_newline = !terminated;
                }
                Err(error) if !terminated && error.is_eof() && !events.is_empty() => {
                    repair = Some(offset);
                    break;
                }
                Err(error) => {
                    return Err(Error::msg(format!(
                        "{} at byte {offset}: {error}",
                        path.display()
                    )))
                }
            }
            offset += n as u64;
        }
        drop(reader);
        if let Some(offset) = repair {
            let backup =
                path.with_extension(format!("jsonl.torn-{}", uuid::Uuid::new_v4().simple()));
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let mut saved = opts.open(&backup)?;
            file.seek(SeekFrom::Start(0))?;
            std::io::copy(&mut file, &mut saved)?;
            saved.sync_all()?;
            file.set_len(offset)?;
            file.sync_all()?;
            eprintln!(
                "hyper: recovered incomplete session tail; backup {}",
                backup.display()
            );
        } else if needs_newline {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        Ok(events)
    })();
    let _ = file.unlock();
    result
}

/// One JSONL line. Folded tool output is ~12k; 8MiB is a hard stop so a
/// single unfolded dump cannot OOM the next resume.
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
/// Whole session file. Compact should keep overnight jsonl far under this;
/// if compact failed, refuse the load instead of allocating gigabytes.
const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;
/// Official compact sidecar is ciphertext JSON, not a user text file. The 8MiB
/// Read slurp cap would drop an overnight blob and force a re-compact loop.
const OFFICIAL_MAX_BYTES: u64 = 32 * 1024 * 1024;

fn read_record(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<usize> {
    line.clear();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(line.len());
        }
        let n = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |i| i + 1);
        if line.len().saturating_add(n) > MAX_RECORD_BYTES {
            return Err(std::io::Error::other(
                "session event exceeds 8 MiB; source preserved",
            ));
        }
        let done = chunk[n - 1] == b'\n';
        line.extend_from_slice(&chunk[..n]);
        reader.consume(n);
        if done {
            return Ok(line.len());
        }
    }
}
