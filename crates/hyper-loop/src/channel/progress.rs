//! IM liveness without changing the agent loop.
//!
//! The coding hop still runs xhigh / tools / compact. Live `EventSink` deltas
//! (the same stream the console think panel uses) are coalesced into a few
//! chat lines so WeChat/QQ do not look frozen.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::session::{DeltaChannel, SessionEvent, ToolLifecyclePhase};

/// Labels for IM ACK / heartbeat / think flush. Harness-side; does not
/// change `drive()`. CJK inbound → Chinese; otherwise English.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImLocale {
    Zh,
    En,
}

impl ImLocale {
    pub fn detect(text: &str) -> Self {
        if text.chars().any(is_cjk) {
            Self::Zh
        } else {
            Self::En
        }
    }

    pub fn ack(self) -> &'static str {
        match self {
            Self::Zh => ACK_TEXT,
            Self::En => "Got it, working on it…",
        }
    }

    pub fn queue_ack(self) -> &'static str {
        match self {
            Self::Zh => QUEUE_ACK,
            Self::En => "Got it — I'll reply when the current task finishes…",
        }
    }

    pub fn steer_ack(self) -> &'static str {
        match self {
            Self::Zh => STEER_ACK,
            Self::En => "Got it, injected into the current task…",
        }
    }

    pub fn overflow_ack(self) -> &'static str {
        match self {
            Self::Zh => OVERFLOW_ACK,
            Self::En => "Queue is full; this one didn't land. Try again after the current task.",
        }
    }

    pub fn heartbeat(self) -> &'static str {
        match self {
            Self::Zh => HEARTBEAT_TEXT,
            Self::En => "Still working…",
        }
    }

    /// Hermes interim-assistant preview: the answer draft as it streams.
    fn reply_label(self) -> &'static str {
        match self {
            Self::Zh => "回复中",
            Self::En => "Replying",
        }
    }

    fn think_label(self) -> &'static str {
        match self {
            Self::Zh => "思考中",
            Self::En => "Thinking",
        }
    }

    fn tool_label(self, name: &str) -> String {
        if self == Self::En {
            return name.to_string();
        }
        match name {
            "Read" | "read" => "读取".into(),
            "Write" | "write" => "写入".into(),
            "StrReplace" => "修改".into(),
            "Delete" | "delete" => "删除".into(),
            "Grep" | "grep" => "搜索".into(),
            "Glob" | "glob" => "找文件".into(),
            "Search" | "search" => "定位".into(),
            "Shell" | "bash" => "命令".into(),
            "WebSearch" | "web_search" => "联网搜".into(),
            "WebFetch" => "打开网页".into(),
            "Task" | "task" => "子任务".into(),
            other => other.to_string(),
        }
    }
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{3000}'..='\u{303F}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{FF00}'..='\u{FFEF}'
    )
}

fn zh_majority(s: &str) -> bool {
    let cjk = s.chars().filter(|c| is_cjk(*c)).count();
    let lat = s.chars().filter(|c| c.is_ascii_alphabetic()).count();
    cjk > 0 && cjk >= lat
}

/// Sentence-ish cuts so a mixed CoT line can keep its Chinese span.
fn think_spans(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in line.char_indices() {
        let rest = &line[i + c.len_utf8()..];
        let ascii_end =
            matches!(c, '.' | '!' | '?' | ';') && (rest.starts_with(' ') || rest.is_empty());
        let dash = c == '-' && i > 0 && line[..i].ends_with(' ') && rest.starts_with(' ');
        let cjk_end = matches!(c, '。' | '！' | '？' | '；');
        if ascii_end || dash || cjk_end {
            let part = line[start..i].trim();
            if !part.is_empty() {
                out.push(part);
            }
            start = i + c.len_utf8();
        }
    }
    let rest = line[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// 中文 IM 滤英文思考：只保留 CJK 占优的行和片段，拉丁占优的丢掉（不翻译）。
/// Keep CJK-majority lines, and CJK spans inside mixed English CoT.
/// Never keep Latin-majority text (no translation through the coding loop).
fn zh_think_keep(s: &str, user: &str) -> String {
    let mut kept = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || is_im_card_echo(line) || is_user_echo(line, user) {
            continue;
        }
        if zh_majority(line) {
            kept.push(line.to_string());
            continue;
        }
        for part in think_spans(line) {
            let part = part.trim();
            if zh_majority(part) && !is_im_card_echo(part) && !is_user_echo(part, user) {
                kept.push(part.to_string());
            }
        }
    }
    kept.join("\n")
}

/// CoT that only restates the inbound turn is not user-facing thinking.
fn is_user_echo(s: &str, user: &str) -> bool {
    let t = s.trim();
    if user.is_empty() || t.chars().count() < 6 {
        return false;
    }
    let compact = |x: &str| x.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let line = compact(t);
    let inbound = compact(user);
    !line.is_empty() && inbound.contains(&line)
}

/// Sticky `[im]` card restated in CoT is not user-facing thinking.
fn is_im_card_echo(s: &str) -> bool {
    let t = s.trim().trim_start_matches("[im]").trim();
    if t.chars().count() < 6 {
        return false;
    }
    IM_CARD_ECHO
        .iter()
        .any(|frag| t == *frag || t.contains(frag) || frag.contains(t))
}

const IM_CARD_ECHO: &[&str] = &[
    "思考过程和回复都必须用中文",
    "不要用英文写思考",
    "工具跳可见正文留空",
    "没有工具的那一跳才是回复",
    "用户已给出路径就直接 Write",
    "同一符号只 Search 一次",
    "同一符号只用 Grep 定位一次",
    "命中后用 Search 给出的片段",
    "命中后用给出的片段",
    "不要整文件 Read",
    "不要整文件 Read 或 cat",
    "不要 Glob 已经 Search 到的同名文件",
    "同一跳并行 Write 和 Shell",
    "不要对整仓 git diff",
    "find、ls -R、tree",
    "用户写中文时",
    "思考过程必须是中文",
    "禁止英文推理",
    "Think and speak in the user's language",
    "Tool hops keep visible text empty",
    "The hop without tools is the answer",
    "do not Glob to confirm",
    "Do not Search paraphrases",
    "Locate a symbol once with Grep",
    "do not re-Grep paraphrases",
    "do not Read the whole file",
    "do not Read or Shell cat",
    "Do not Glob a filename Search already located",
    "Do not Glob a filename Grep already located",
    "不要 Glob 已经定位到的同名文件",
    "send them in the same hop",
    "Do not Shell git diff",
    "Do not Glob **/*",
    "这一跳思考用中文",
    "This hop: think in the user's language",
    "整段只回 NO_REPLY",
    "reply with exactly NO_REPLY",
];

pub const ACK_TEXT: &str = "收到，正在处理…";
/// Mid-turn follow-up under queue: ACK now, answer after the live turn.
pub const QUEUE_ACK: &str = "收到，当前任务结束后回复…";
pub const STEER_ACK: &str = "收到，已插入当前任务…";
pub const OVERFLOW_ACK: &str = "队列已满，这条先没排上。当前任务结束后可以再发。";
pub const HEARTBEAT_TEXT: &str = "还在处理…";
pub const THINK_FLUSH: Duration = Duration::from_secs(12);
pub const TOOL_GAP: Duration = Duration::from_secs(4);
pub const HEARTBEAT: Duration = Duration::from_secs(20);
pub const THINK_TAIL: usize = 1200;
/// Answer-draft preview tail; the full text still arrives as the final reply.
pub const CONTENT_TAIL: usize = 600;
/// Append-only channels (QQ / WeChat / DingTalk) cannot edit a bubble: a
/// short draft posted early would repeat right above the final reply. Only
/// stack a draft preview once the answer is long enough to be worth it.
pub const STACK_DRAFT_MIN: usize = 400;
pub const MIN_THINK_FLUSH: usize = 40;
/// CJK sentences are short; 40 latin chars would hide Chinese CoT all turn.
pub const MIN_THINK_FLUSH_ZH: usize = 16;
/// Marker: English CoT was stubbed to the Chinese think label only.
const EN_THINK_STUB: &str = "\u{0001}en";
/// QQ Bot passive replies expire; after this, drop `msg_id` and send actively.
pub const PASSIVE_REPLY_TTL: Duration = Duration::from_secs(240);

#[derive(Debug)]
pub struct ProgressBuf {
    locale: ImLocale,
    think: String,
    last_sent: String,
    last_think: String,
    last_think_at: Instant,
    last_tool_at: Instant,
    last_beat_at: Instant,
    /// QQ/WeChat already show typing; extra heartbeat lines spam (no edit API).
    text_heartbeat: bool,
    /// QQ / WeChat / DingTalk cannot edit a bubble. Hold think+tool and
    /// emit one chat line per THINK_FLUSH instead of a new message per hop.
    stack_lines: bool,
    pending: String,
    last_stack_at: Instant,
    stack_sent: bool,
    /// A tool progress line already proved liveness this turn.
    posted_tool: bool,
    /// Feishu: ACK + Typing reaction cover first-hop wait; skip 「还在处理…」.
    ack_covers_wait: bool,
    /// Feishu text edits cap at 20. First tool line is immediate; later hops
    /// wait until `THINK_FLUSH` so a Read storm does not burn the cap.
    pace_edits: bool,
    last_emit_at: Instant,
    /// Latest tool status line; Feishu snapshot keeps it above Chinese think.
    last_tool_line: String,
    /// New runtimes announce `scheduled` before the assistant tool-call event.
    /// Legacy transcripts without lifecycle still use the assistant fallback.
    typed_tools: bool,
    /// Inbound user text; CoT that only restates it is dropped.
    user_text: String,
    /// Visible answer text streaming in on DeltaChannel::Content. Shown as a
    /// live preview (Hermes interim messages); the final reply is unchanged.
    content: String,
    /// Last content section actually posted, for dedupe.
    content_shown: String,
}

impl ProgressBuf {
    pub fn new(now: Instant) -> Self {
        Self::with_locale(now, ImLocale::Zh)
    }

    pub fn for_user(now: Instant, inbound: &str) -> Self {
        let mut buf = Self::with_locale(now, ImLocale::detect(inbound));
        buf.user_text = inbound.to_string();
        buf
    }

    pub fn for_channel(now: Instant, inbound: &str, channel: &str) -> Self {
        let mut buf = Self::for_user(now, inbound);
        buf.text_heartbeat = !typing_covers_quiet(channel);
        buf.stack_lines = stack_progress(channel);
        buf.ack_covers_wait = channel.eq_ignore_ascii_case("feishu");
        buf.pace_edits = channel.eq_ignore_ascii_case("feishu");
        buf
    }

    pub fn with_locale(now: Instant, locale: ImLocale) -> Self {
        Self {
            locale,
            think: String::new(),
            last_sent: String::new(),
            last_think: String::new(),
            last_think_at: now,
            last_tool_at: now - TOOL_GAP,
            last_beat_at: now,
            text_heartbeat: true,
            stack_lines: false,
            pending: String::new(),
            last_stack_at: now,
            stack_sent: false,
            posted_tool: false,
            ack_covers_wait: false,
            pace_edits: false,
            last_emit_at: now,
            last_tool_line: String::new(),
            typed_tools: false,
            user_text: String::new(),
            content: String::new(),
            content_shown: String::new(),
        }
    }

    pub fn ingest(&mut self, ev: &SessionEvent, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        match ev {
            SessionEvent::Delta(d) if d.reset => {
                if !d.content_only {
                    if let Some(line) = self.flush_think(now, false) {
                        out.push(line);
                    }
                    self.think.clear();
                }
                // New step or a retracted answer bubble (tool hop after
                // visible text): the old draft preview is stale either way.
                self.content.clear();
                self.content_shown.clear();
            }
            SessionEvent::Delta(d) if d.channel == DeltaChannel::Reasoning => {
                if is_prepare_hint(&d.text) {
                    return self.stack_out(out, now, false);
                }
                self.think.push_str(&d.text);
                if let Some(line) = self.flush_think(now, false) {
                    out.push(line);
                }
            }
            SessionEvent::Delta(d) if d.channel == DeltaChannel::Content => {
                // Accumulate only; emission rides tick()/stack pacing so a
                // token stream never floods the chat.
                self.content.push_str(&d.text);
            }
            SessionEvent::Delta(_) => {}
            SessionEvent::Assistant(a) => {
                if !self.typed_tools {
                    if let Some(calls) = &a.tool_calls {
                        if self.stack_lines {
                            if let Some(line) = self.flush_think(now, true) {
                                out.push(line);
                            }
                        } else if !self.pace_edits {
                            self.think.clear();
                        }
                        if let Some(line) = tool_summary(self.locale, calls) {
                            if let Some(posted) = self.post_tool_line(line, now) {
                                out.push(posted);
                            }
                        }
                    }
                }
            }
            SessionEvent::ToolLifecycle(tool) => {
                self.typed_tools = true;
                if tool.phase == ToolLifecyclePhase::Scheduled {
                    return self.stack_out(out, now, false);
                }
                if self.stack_lines && tool.phase == ToolLifecyclePhase::Started {
                    if let Some(line) = self.flush_think(now, true) {
                        out.push(line);
                    }
                } else if !self.pace_edits && tool.phase == ToolLifecyclePhase::Started {
                    self.think.clear();
                }
                let label = self.locale.tool_label(&tool.name);
                let detail = tool.summary.as_deref().unwrap_or("");
                let line = match (self.locale, tool.phase) {
                    (ImLocale::Zh, ToolLifecyclePhase::Started) if !detail.is_empty() => {
                        format!("[{label}] {}", clip_chars(detail, 96))
                    }
                    (ImLocale::En, ToolLifecyclePhase::Started) if !detail.is_empty() => {
                        format!("[{label}] {}", clip_chars(detail, 96))
                    }
                    (ImLocale::Zh, ToolLifecyclePhase::Started) => format!("[{label}] 进行中"),
                    (ImLocale::En, ToolLifecyclePhase::Started) => format!("[{label}] running"),
                    (ImLocale::Zh, ToolLifecyclePhase::Completed) => format!("[{label}] 完成"),
                    (ImLocale::En, ToolLifecyclePhase::Completed) => format!("[{label}] done"),
                    (ImLocale::Zh, ToolLifecyclePhase::Skipped) => {
                        format!("[{label}] 已跳过（收到新指令）")
                    }
                    (ImLocale::En, ToolLifecyclePhase::Skipped) => {
                        format!("[{label}] skipped for steering")
                    }
                    (ImLocale::Zh, ToolLifecyclePhase::Interrupted) => format!("[{label}] 已中断"),
                    (ImLocale::En, ToolLifecyclePhase::Interrupted) => {
                        format!("[{label}] interrupted")
                    }
                    (ImLocale::Zh, ToolLifecyclePhase::Error) => format!("[{label}] 失败"),
                    (ImLocale::En, ToolLifecyclePhase::Error) => format!("[{label}] failed"),
                    (_, ToolLifecyclePhase::Scheduled) => unreachable!("handled above"),
                };
                if let Some(posted) = self.post_tool_line(line, now) {
                    out.push(posted);
                }
            }
            SessionEvent::Tool(t) => {
                if t.name.starts_with("web_search")
                    || t.name == "x_search"
                    || t.name == "web_search"
                {
                    let label = self.locale.tool_label(&t.name);
                    let line = if t.output.is_empty() {
                        format!("[{label}]")
                    } else {
                        format!("[{label}] {}", clip_chars(&t.output, 80))
                    };
                    if let Some(posted) = self.post_tool_line(line, now) {
                        out.push(posted);
                    }
                }
            }
            _ => {}
        }
        self.stack_out(out, now, false)
    }

    pub fn tick(&mut self, now: Instant) -> Vec<String> {
        let out: Vec<String> = self.flush_think(now, false).into_iter().collect();
        let mut stacked = self.stack_out(out, now, false);
        if stacked.is_empty() {
            if let Some(line) = self.flush_paced(now) {
                stacked = vec![line];
            }
        }
        if stacked.is_empty() {
            if let Some(line) = self.flush_content(now) {
                stacked = vec![line];
            }
        }
        if stacked.is_empty() && self.pending.is_empty() {
            if let Some(line) = self.heartbeat(now) {
                return vec![line];
            }
        }
        stacked
    }

    /// Answer-draft section under the tool/think lines. None while empty.
    fn content_section(&self) -> Option<String> {
        let t = self.content.trim();
        if t.is_empty() {
            return None;
        }
        // 沉默 token 逐字流入（"N" → "NO" → … → "NO_REPLY"）。草稿若原样
        // 外播，QQ/微信/钉钉会立刻发出「回复中\nNO_REPLY」，终稿又被
        // is_silence 吞掉，群里就看到沉默 token。命中沉默前缀就不渲染。
        if super::is_silence_prefix(&super::normalize_silence(t)) {
            return None;
        }
        Some(format!(
            "{}\n{}",
            self.locale.reply_label(),
            think_tail(t, CONTENT_TAIL)
        ))
    }

    /// Append the live answer draft under a tool/think bubble.
    fn with_content(&self, base: String) -> String {
        match self.content_section() {
            Some(sec) if base.is_empty() => sec,
            Some(sec) => format!("{base}\n\n{sec}"),
            None => base,
        }
    }

    /// Edit-in-place channels: the answer draft refreshes the bubble on the
    /// same THINK_FLUSH cadence as tool lines (Feishu edit cap applies).
    /// Stack channels get their draft through `stack_out` instead.
    fn flush_content(&mut self, now: Instant) -> Option<String> {
        if self.stack_lines || self.content.is_empty() || self.content == self.content_shown {
            return None;
        }
        if now.duration_since(self.last_emit_at) < THINK_FLUSH {
            return None;
        }
        let base = if self.posted_tool {
            self.tool_think_snapshot(&self.last_tool_line.clone())
        } else {
            String::new()
        };
        let line = self.with_content(base);
        if line.is_empty() || line == self.last_sent {
            return None;
        }
        self.content_shown = self.content.clone();
        self.note_posted(&line);
        self.last_emit_at = now;
        Some(line)
    }

    fn post_tool_line(&mut self, line: String, now: Instant) -> Option<String> {
        if line == self.last_tool_line {
            return None;
        }
        self.last_tool_at = now;
        self.last_tool_line = line.clone();
        let first = !self.posted_tool;
        self.posted_tool = true;
        if self.pace_edits && !first {
            self.pending = line;
            return self.flush_paced(now);
        }
        let snap = self.with_content(self.tool_think_snapshot(&line));
        self.note_posted(&snap);
        self.content_shown = self.content.clone();
        self.last_emit_at = now;
        Some(snap)
    }

    fn flush_paced(&mut self, now: Instant) -> Option<String> {
        if !self.pace_edits || !self.posted_tool {
            return None;
        }
        if now.duration_since(self.last_emit_at) < THINK_FLUSH {
            return None;
        }
        let tool = if !self.pending.is_empty() {
            std::mem::take(&mut self.pending)
        } else if !self.last_tool_line.is_empty() {
            self.last_tool_line.clone()
        } else {
            return None;
        };
        let line = self.with_content(self.tool_think_snapshot(&tool));
        if line == self.last_sent {
            return None;
        }
        self.note_posted(&line);
        self.content_shown = self.content.clone();
        self.last_emit_at = now;
        Some(line)
    }

    /// Hermes-style: tool status on top, Chinese think underneath. English CoT
    /// is dropped so a Read storm does not spend edits on Latin restatement.
    fn tool_think_snapshot(&self, tool: &str) -> String {
        let tool = tool.trim();
        let painted = crate::think_visible::visible_think(&self.think);
        let kept = if self.locale == ImLocale::Zh {
            zh_think_keep(&painted, &self.user_text)
        } else {
            think_tail(&painted, THINK_TAIL)
        };
        let min = if self.locale == ImLocale::Zh {
            MIN_THINK_FLUSH_ZH
        } else {
            MIN_THINK_FLUSH
        };
        if char_len(&kept) < min {
            return tool.to_string();
        }
        format!(
            "{tool}\n\n{}\n{}",
            self.locale.think_label(),
            think_tail(&kept, THINK_TAIL)
        )
    }

    fn heartbeat(&mut self, now: Instant) -> Option<String> {
        if !self.text_heartbeat {
            return None;
        }
        let quiet = now
            .duration_since(self.last_think_at)
            .min(now.duration_since(self.last_tool_at));
        if quiet < HEARTBEAT {
            return None;
        }
        if now.duration_since(self.last_beat_at) < HEARTBEAT {
            return None;
        }
        // Edit/replace bubbles: ACK + Typing reaction already prove liveness.
        // 「还在处理…」 spends a Feishu edit (cap 20) and overwrites the ACK.
        if !self.stack_lines && self.posted_tool {
            return None;
        }
        if !self.stack_lines && !self.last_think.is_empty() && self.last_think != EN_THINK_STUB {
            return None;
        }
        // A live answer draft in the bubble is better proof of life than
        // 「还在处理…」; never patch over it.
        if !self.stack_lines && !self.content.is_empty() {
            return None;
        }
        if self.ack_covers_wait && !self.posted_tool {
            return None;
        }
        let beat = self.locale.heartbeat();
        self.last_sent = beat.into();
        self.last_beat_at = now;
        Some(beat.into())
    }

    pub fn finish(&mut self, now: Instant) -> Vec<String> {
        if !self.stack_lines {
            // Telegram / Feishu / WeCom / webhook edit one bubble into the
            // final reply. Leftover CoT after the hop would overwrite it.
            return Vec::new();
        }
        let lines: Vec<String> = self.flush_think(now, true).into_iter().collect();
        self.stack_out(lines, now, true)
    }

    /// Edit-in-place channels: the final reply goes out as a new message
    /// while the old progress bubble keeps whatever draft was last shown.
    /// After `finish`, collapse that bubble back to its bare tool/think
    /// snapshot (or the ACK, when no tool line ran) so a long answer is
    /// not readable twice (「回复中」 head above, full reply below).
    /// Stack channels cannot edit; short drafts there are already held
    /// back by STACK_DRAFT_MIN.
    pub fn collapse_draft(&mut self) -> Option<String> {
        if self.stack_lines || self.content_shown.is_empty() {
            return None;
        }
        let snap = if self.posted_tool {
            self.tool_think_snapshot(&self.last_tool_line.clone())
        } else {
            // 没工具行可留：收回 ACK，避免写成空泡，也避免纯聊天长回答
            // 上面「回复中」、下面终稿叠两层。
            self.locale.ack().to_string()
        };
        if snap.is_empty() || snap == self.last_sent {
            return None;
        }
        self.content.clear();
        self.content_shown.clear();
        self.note_posted(&snap);
        Some(snap)
    }

    fn push_pending(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.pending.lines().last() == Some(line) {
            return;
        }
        if self.pending.is_empty() {
            self.pending = line.to_string();
        } else {
            self.pending.push('\n');
            self.pending.push_str(line);
        }
        if char_len(&self.pending) > THINK_TAIL {
            self.pending = think_tail(&self.pending, THINK_TAIL);
        }
    }

    /// Edit/replace channels record the last posted line immediately.
    /// Append-only chats leave `last_sent` to `stack_out` so a combined
    /// think+tool snapshot is not dropped as a duplicate of the tool bit.
    fn note_posted(&mut self, line: &str) {
        if !self.stack_lines {
            self.last_sent = line.to_string();
        }
    }

    /// Append-only chats: first progress line goes out; later think/tool
    /// lines wait until THINK_FLUSH or finish so QQ/WeChat do not flood.
    /// The streaming answer draft rides the same batch — but never in a
    /// `force` (finish) flush, where the final reply is already imminent.
    fn stack_out(&mut self, lines: Vec<String>, now: Instant, force: bool) -> Vec<String> {
        if !self.stack_lines {
            return lines;
        }
        for line in lines {
            self.push_pending(&line);
        }
        let due =
            force || !self.stack_sent || now.duration_since(self.last_stack_at) >= THINK_FLUSH;
        // Append-only channels cannot edit the draft away later; a short
        // answer would repeat right above the final reply. Hold the draft
        // until it is long enough to be worth a second bubble.
        let fresh = !force
            && char_len(&self.content) >= STACK_DRAFT_MIN
            && self.content_section().is_some_and(|s| s != self.content_shown);
        if !due {
            return Vec::new();
        }
        if self.pending.is_empty() {
            if fresh {
                return vec![self.mark_content_shown(now)];
            }
            return Vec::new();
        }
        if self.pending == self.last_sent {
            self.pending.clear();
            if fresh {
                return vec![self.mark_content_shown(now)];
            }
            return Vec::new();
        }
        let mut line = std::mem::take(&mut self.pending);
        if fresh {
            let sec = self.content_section().expect("fresh checked");
            line.push_str("\n\n");
            line.push_str(&sec);
            self.content_shown = sec;
        }
        self.last_sent = line.clone();
        self.last_stack_at = now;
        self.stack_sent = true;
        vec![line]
    }

    fn mark_content_shown(&mut self, now: Instant) -> String {
        let sec = self.content_section().unwrap_or_default();
        self.content_shown = sec.clone();
        self.last_sent = sec.clone();
        self.last_stack_at = now;
        self.stack_sent = true;
        sec
    }

    fn flush_think(&mut self, now: Instant, force: bool) -> Option<String> {
        if !self.stack_lines && self.posted_tool {
            if self.pace_edits {
                return None;
            }
            self.think.clear();
            return None;
        }
        let painted = crate::think_visible::visible_think(&self.think);
        let n = char_len(&painted);
        let min = if self.locale == ImLocale::Zh {
            MIN_THINK_FLUSH_ZH
        } else {
            MIN_THINK_FLUSH
        };
        if n < min {
            return None;
        }
        if !force && now.duration_since(self.last_think_at) < THINK_FLUSH {
            return None;
        }
        // Chinese IM: never dump English CoT. Keep CJK spans from mixed
        // lines. Bare 「思考中」 is not thinking in the user's language —
        // ACK, tool lines, and heartbeat already prove liveness.
        if self.locale == ImLocale::Zh {
            let kept = zh_think_keep(&painted, &self.user_text);
            if kept.is_empty() {
                self.think.clear();
                self.last_think = EN_THINK_STUB.into();
                return None;
            }
            let body = think_tail(&kept, THINK_TAIL);
            if body == self.last_think {
                return None;
            }
            let line = format!("{}\n{}", self.locale.think_label(), body);
            if line == self.last_sent {
                return None;
            }
            self.last_think = body;
            self.note_posted(&line);
            self.last_think_at = now;
            return Some(line);
        }
        let body = think_tail(&painted, THINK_TAIL);
        // Tool lines overwrite last_sent; still skip the same CoT blob.
        if body == self.last_think {
            return None;
        }
        let line = format!("{}\n{}", self.locale.think_label(), body);
        if line == self.last_sent {
            return None;
        }
        self.last_think = body;
        self.note_posted(&line);
        self.last_think_at = now;
        Some(line)
    }
}

/// QQ C2C `input_notify` / WeChat iLink typing already prove the bot is alive.
/// Those chats cannot edit a progress bubble, so text heartbeats just stack.
pub fn typing_covers_quiet(channel: &str) -> bool {
    channel.eq_ignore_ascii_case("qq") || channel.eq_ignore_ascii_case("wechat")
}

/// No in-place progress bubble. Coalesce instead of one native message per hop.
pub fn stack_progress(channel: &str) -> bool {
    channel.eq_ignore_ascii_case("qq")
        || channel.eq_ignore_ascii_case("wechat")
        || channel.eq_ignore_ascii_case("dingtalk")
}

pub fn is_prepare_hint(text: &str) -> bool {
    let t = text.trim();
    t == "正在连接模型…"
        || t == "正在整理上下文…"
        || t == "正在准备工作区…"
        || t.starts_with("网络不稳")
}

pub fn think_tail(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if char_len(t) <= max_chars {
        return t.to_string();
    }
    let mut n = 0usize;
    let mut start = t.len();
    for (i, _) in t.char_indices().rev() {
        n += 1;
        start = i;
        if n >= max_chars.saturating_sub(1) {
            break;
        }
    }
    format!("…{}", &t[start..])
}

fn tool_summary(locale: ImLocale, calls: &[crate::session::OpenAiToolCall]) -> Option<String> {
    let mut bits = Vec::new();
    for c in calls.iter().take(3) {
        bits.push(tool_bit(locale, &c.function.name, &c.function.arguments));
    }
    if bits.is_empty() {
        return None;
    }
    Some(bits.join(" · "))
}

fn tool_bit(locale: ImLocale, name: &str, args: &str) -> String {
    let v: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let preview = match name {
        "Read" | "read" | "Write" | "write" | "StrReplace" | "Delete" | "delete" => s("path"),
        "Grep" | "grep" => s("pattern"),
        "Glob" | "glob" => s("glob_pattern"),
        "Search" | "search" => s("query"),
        "Shell" | "bash" => s("command"),
        "WebSearch" => s("search_term"),
        "WebFetch" => s("url"),
        "Task" | "task" => s("description"),
        _ => "",
    };
    let label = locale.tool_label(name);
    if preview.is_empty() {
        label
    } else {
        format!("{label} {}", clip_chars(preview, 48))
    }
}

fn clip_chars(s: &str, n: usize) -> String {
    let t = s.trim().replace('\n', " ");
    if char_len(&t) <= n {
        return t;
    }
    format!(
        "{}…",
        t.chars().take(n.saturating_sub(1)).collect::<String>()
    )
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{DeltaEvent, OpenAiToolCall};
    use serde_json::json;
    use std::time::Duration;

    fn reasoning(text: &str) -> SessionEvent {
        SessionEvent::Delta(DeltaEvent {
            channel: DeltaChannel::Reasoning,
            text: text.into(),
            delta: true,
            reset: false,
            content_only: false,
        })
    }

    fn content(text: &str) -> SessionEvent {
        SessionEvent::Delta(DeltaEvent {
            channel: DeltaChannel::Content,
            text: text.into(),
            delta: true,
            reset: false,
            content_only: false,
        })
    }

    #[test]
    fn answer_draft_stacks_on_append_only_chats() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "帮我写个周报", "qq");
        // 草稿要长过 STACK_DRAFT_MIN 才会在 append-only 渠道提前外播；
        // 短回答直接等终稿，避免「回复中」和终稿重复。
        let draft = "本周完成了 IM 体验改造，包括分段、防抖、流式预览。".repeat(17);
        // First visible text goes out right away so QQ does not look frozen.
        let first = b.ingest(&content(&draft), t0);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].starts_with("回复中\n"), "{}", first[0]);
        assert!(first[0].contains("本周完成"), "{}", first[0]);
        // More tokens inside the pacing window do not flood the chat.
        assert!(b.ingest(&content("继续写。"), t0 + Duration::from_secs(1)).is_empty());
        let next = b.tick(t0 + THINK_FLUSH);
        assert_eq!(next.len(), 1, "{next:?}");
        assert!(next[0].contains("继续写"), "{next:?}");
    }

    #[test]
    fn short_draft_waits_for_the_final_reply_on_append_only_chats() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "帮我改标题", "qq");
        assert!(
            b.ingest(&content("已改好。"), t0).is_empty(),
            "append-only chats cannot edit; a short draft would duplicate the final reply"
        );
        assert!(b.tick(t0 + THINK_FLUSH).is_empty());
    }

    #[test]
    fn answer_draft_rides_the_edit_bubble() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "帮我改标题", "feishu");
        assert_eq!(b.ingest(&read_tool("src/main.rs"), t0), vec!["读取 src/main.rs"]);
        let draft = "已把标题改成更短的版本。".repeat(4);
        assert!(b.ingest(&content(&draft), t0 + Duration::from_secs(1)).is_empty());
        let lines = b.tick(t0 + THINK_FLUSH);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("读取 src/main.rs"), "{}", lines[0]);
        assert!(lines[0].contains("回复中\n"), "{}", lines[0]);
        assert!(lines[0].contains("已把标题改成"), "{}", lines[0]);
        // Unchanged draft does not spend another Feishu edit.
        assert!(b.tick(t0 + THINK_FLUSH + THINK_FLUSH).is_empty());
    }

    #[test]
    fn hop_reset_drops_the_old_draft() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "写个文件", "qq");
        let draft = "第一跳的正文。".repeat(60);
        assert_eq!(b.ingest(&content(&draft), t0).len(), 1);
        b.ingest(
            &SessionEvent::Delta(DeltaEvent {
                channel: DeltaChannel::Content,
                text: String::new(),
                delta: true,
                reset: true,
                content_only: false,
            }),
            t0 + Duration::from_secs(1),
        );
        assert!(
            b.tick(t0 + THINK_FLUSH).is_empty(),
            "reset hop must not replay the previous draft"
        );
    }

    #[test]
    fn finish_flush_drops_draft_before_final_reply() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "写个文件", "wechat");
        b.ingest(&content("最终答案全文。"), t0);
        let out = b.finish(t0 + Duration::from_secs(2));
        assert!(
            !out.iter().any(|l| l.contains("回复中")),
            "the final reply follows immediately; draft preview must not duplicate: {out:?}"
        );
    }

    #[test]
    fn silence_token_never_leaks_into_the_draft_preview() {
        let t0 = Instant::now();
        // Append-only: 沉默 token 一到就发出去的话，终稿拦截就晚了。
        let mut b = ProgressBuf::for_channel(t0, "群里闲聊", "qq");
        assert!(b.ingest(&content("NO"), t0).is_empty(), "prefix of NO_REPLY");
        assert!(b.ingest(&content("_REPLY"), t0).is_empty(), "NO_REPLY complete");
        assert!(b.tick(t0 + THINK_FLUSH).is_empty(), "tick must not flush it");
        // 编辑类渠道（飞书 tick 刷草稿）同样不渲染沉默段。
        let mut f = ProgressBuf::for_channel(t0, "群里闲聊", "feishu");
        f.ingest(&content("NO_REPLY"), t0);
        assert!(
            f.tick(t0 + THINK_FLUSH).is_empty(),
            "feishu tick must not paint NO_REPLY into the bubble"
        );
        // 以 No 开头的正常回答只被压住前几个 token，成形后照常预览。
        let mut n = ProgressBuf::for_channel(t0, "帮我写个回答", "feishu");
        n.ingest(&content("No problem — here is the full answer.".repeat(12).as_str()), t0);
        let lines = n.tick(t0 + THINK_FLUSH);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("No problem"), "{}", lines[0]);
    }

    #[test]
    fn collapse_draft_strips_the_reply_section_off_the_old_bubble() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "帮我改标题", "feishu");
        assert_eq!(b.ingest(&read_tool("src/main.rs"), t0), vec!["读取 src/main.rs"]);
        let draft = "已把标题改成更短的版本。".repeat(30);
        assert!(b.ingest(&content(&draft), t0 + Duration::from_secs(1)).is_empty());
        let lines = b.tick(t0 + THINK_FLUSH);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("回复中\n"), "{}", lines[0]);
        let collapsed = b.collapse_draft().expect("draft was shown; collapse it");
        assert!(collapsed.contains("读取 src/main.rs"), "{collapsed}");
        assert!(!collapsed.contains("回复中"), "{collapsed}");
        assert!(!collapsed.contains("已把标题"), "{collapsed}");
        // 没播过草稿（短回答被阈值挡住/编辑渠道未刷过）时无事可做。
        let mut quiet = ProgressBuf::for_channel(t0, "帮我改标题", "feishu");
        quiet.ingest(&read_tool("src/main.rs"), t0);
        assert!(quiet.collapse_draft().is_none());
        // 纯聊天、生成超过 12s：旧泡里只有「回复中」，收回 ACK。
        let mut chat = ProgressBuf::for_channel(t0, "在吗", "feishu");
        chat.ingest(&content(&"你好，我在。".repeat(40)), t0);
        assert_eq!(chat.tick(t0 + THINK_FLUSH).len(), 1);
        let collapsed = chat.collapse_draft().expect("draft-only bubble");
        assert_eq!(collapsed, ACK_TEXT);
        assert!(!collapsed.contains("回复中"), "{collapsed}");
    }

    #[test]
    fn heartbeat_never_patches_over_answer_draft() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "写个长一点的回答", "telegram");
        b.ingest(&content("正文正在生成…"), t0);
        let first = b.tick(t0 + THINK_FLUSH);
        assert_eq!(first.len(), 1, "{first:?}");
        let later = b.tick(t0 + THINK_FLUSH + HEARTBEAT + HEARTBEAT);
        assert!(
            later.is_empty(),
            "draft bubble is liveness; 还在处理 must not replace it: {later:?}"
        );
    }

    #[test]
    fn prepare_hints_are_silent() {
        assert!(is_prepare_hint("正在连接模型…"));
        assert!(is_prepare_hint("网络不稳，正在重连（第1次，1s 后）…\n"));
        let now = Instant::now();
        let mut b = ProgressBuf::new(now);
        assert!(b.ingest(&reasoning("正在连接模型…"), now).is_empty());
    }

    #[test]
    fn think_flushes_after_interval() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::new(t0);
        let chunk = "计划先读入口再改标题。".repeat(4);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let later = t0 + THINK_FLUSH;
        let lines = b.ingest(&reasoning("继续。"), later);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("思考中\n"), "{}", lines[0]);
        assert!(lines[0].contains("计划先读"));
    }

    #[test]
    fn english_think_is_not_posted_on_chinese_inbound() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(t0, "帮我改标题");
        let chunk = "The user wants me to read the entry then edit the title.".repeat(3);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let lines = b.ingest(&reasoning(" still going."), t0 + THINK_FLUSH);
        assert!(
            lines.is_empty(),
            "empty 思考中 is think waste when CoT is English: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("The user wants")),
            "English CoT must not land in a Chinese IM thread: {lines:?}"
        );
        let again = b.ingest(&reasoning(&chunk), t0 + THINK_FLUSH + THINK_FLUSH);
        assert!(
            again.is_empty(),
            "English CoT must stay silent on Chinese IM: {again:?}"
        );
        let mut en = ProgressBuf::for_user(t0, "fix the title");
        let en_chunk = "I will read the entry then edit the title.".repeat(3);
        assert!(en.ingest(&reasoning(&en_chunk), t0).is_empty());
        let en_lines = en.ingest(&reasoning(" next."), t0 + THINK_FLUSH);
        assert_eq!(en_lines.len(), 1, "{en_lines:?}");
        assert!(en_lines[0].starts_with("Thinking\n"), "{}", en_lines[0]);
    }

    #[test]
    fn mixed_english_line_keeps_chinese_span_only() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(t0, "帮我改标题");
        let mixed = "计划先读入口再改标题 - keep visible text empty on tool hops. Then the last hop without tools is the reply.".repeat(2);
        assert!(b.ingest(&reasoning(&mixed), t0).is_empty());
        let lines = b.ingest(&reasoning(" still."), t0 + THINK_FLUSH);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("思考中\n"), "{}", lines[0]);
        assert!(lines[0].contains("计划先读入口再改标题"), "{}", lines[0]);
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("keep visible") || l.contains("The user")),
            "{lines:?}"
        );
        let mut zh = ProgressBuf::for_user(t0, "帮我改标题");
        let chinese = "计划先读入口再改标题，不扩大范围。".repeat(3);
        let zh_lines = zh.ingest(&reasoning(&format!("{chinese}继续。")), t0 + THINK_FLUSH);
        assert_eq!(zh_lines.len(), 1, "{zh_lines:?}");
        assert!(zh_lines[0].starts_with("思考中\n"), "{}", zh_lines[0]);
        assert!(zh_lines[0].contains("计划先读"));
        assert!(!zh_lines[0].contains("keep visible"));
    }

    #[test]
    fn user_instruction_echo_is_not_posted_as_think() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(
            t0,
            "在 `.grok-hyper/overnight/im_queue.py` 末尾追加一行注释 # IM_QUEUE_MARK。不要改 crates/。中文一句结束。",
        );
        let echo = "The user asked to append a comment. 中文一句结束 keep going on this.".repeat(3);
        assert!(b.ingest(&reasoning(&echo), t0).is_empty());
        let lines = b.ingest(&reasoning(" still going."), t0 + THINK_FLUSH);
        assert!(
            !lines.iter().any(|l| l.contains("中文一句结束")),
            "user-turn restated in CoT must not be posted: {lines:?}"
        );
        assert!(
            lines.is_empty(),
            "user-echo-only CoT must not stub 思考中: {lines:?}"
        );

        let mut real = ProgressBuf::for_user(t0, "在文件末尾追加注释。中文一句结束。");
        let plan = "计划先读入口再改标题，不扩大范围。".repeat(3);
        let kept = real.ingest(&reasoning(&format!("{plan}继续。")), t0 + THINK_FLUSH);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(kept[0].contains("计划先读"), "{kept:?}");
        assert!(!kept[0].contains("中文一句结束"), "{kept:?}");
    }

    #[test]
    fn im_card_echo_is_not_posted_as_think() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(t0, "写个脚本");
        let echo = "工具跳可见正文留空 - keep visible text empty on tool hops.".repeat(3);
        assert!(b.ingest(&reasoning(&echo), t0).is_empty());
        let lines = b.ingest(&reasoning(" still going."), t0 + THINK_FLUSH);
        assert!(
            !lines.iter().any(|l| l.contains("工具跳可见正文留空")),
            "IM card restated in CoT must not be posted: {lines:?}"
        );
        assert!(
            lines.is_empty(),
            "card-echo-only CoT must not stub 思考中: {lines:?}"
        );
    }

    #[test]
    fn tool_hop_becomes_one_line() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::new(t0);
        let ev = SessionEvent::assistant_usage(
            String::new(),
            String::new(),
            Some(vec![OpenAiToolCall::function(
                "c1",
                "Read",
                json!({"path": "src/main.rs"}).to_string(),
            )]),
            0,
            0,
            None,
            None,
        );
        let lines = b.ingest(&ev, t0);
        assert_eq!(lines, vec!["读取 src/main.rs"]);
        let mut en = ProgressBuf::for_user(t0, "read the entry file");
        let en_lines = en.ingest(&ev, t0);
        assert_eq!(en_lines, vec!["Read src/main.rs"]);
    }

    #[test]
    fn duplicate_think_is_not_reposted_after_tool_line() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::new(t0);
        let chunk = "用户要求在脚本末尾追加一行注释。不要改 crates/。".repeat(2);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let first = b.ingest(&reasoning("。"), t0 + THINK_FLUSH);
        assert_eq!(first.len(), 1, "{first:?}");
        let tool = SessionEvent::assistant_usage(
            String::new(),
            String::new(),
            Some(vec![OpenAiToolCall::function(
                "c1",
                "Read",
                json!({"path": ".grok-hyper/overnight/im_queue.py"}).to_string(),
            )]),
            0,
            0,
            None,
            None,
        );
        assert_eq!(
            b.ingest(&tool, t0 + THINK_FLUSH + Duration::from_secs(5)),
            vec!["读取 .grok-hyper/overnight/im_queue.py"]
        );
        let again = b.ingest(&reasoning(""), t0 + THINK_FLUSH + Duration::from_secs(20));
        assert!(
            again.is_empty(),
            "same think must not post twice: {again:?}"
        );
    }

    #[test]
    fn silent_stretch_sends_heartbeat() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::new(t0);
        assert!(b.tick(t0 + Duration::from_secs(4)).is_empty());
        let lines = b.tick(t0 + HEARTBEAT);
        assert_eq!(lines, vec![HEARTBEAT_TEXT]);
        assert!(b.tick(t0 + HEARTBEAT + Duration::from_secs(4)).is_empty());
        let again = b.tick(t0 + HEARTBEAT + HEARTBEAT);
        assert_eq!(
            again,
            vec![HEARTBEAT_TEXT],
            "long quiet must keep beating every {HEARTBEAT:?}"
        );
    }

    #[test]
    fn feishu_english_cot_keeps_ack_not_heartbeat() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "帮我改标题", "feishu");
        let chunk = "The user wants me to audit the grok-hyper workspace.".repeat(3);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let beat = b.tick(t0 + HEARTBEAT);
        assert!(
            beat.is_empty(),
            "Feishu Typing + ACK cover first-hop wait; 还在处理 spends an edit: {beat:?}"
        );
        assert!(
            !beat.iter().any(|l| l.contains("The user wants")),
            "{beat:?}"
        );
    }

    #[test]
    fn feishu_does_not_overwrite_tool_line_with_heartbeat() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "帮我改标题", "feishu");
        assert_eq!(
            b.ingest(&read_tool("src/main.rs"), t0),
            vec!["读取 src/main.rs"]
        );
        let late = b.tick(t0 + HEARTBEAT);
        assert!(
            late.is_empty(),
            "heartbeat must not patch over the tool line: {late:?}"
        );
    }

    #[test]
    fn feishu_coalesces_tool_burst_inside_flush() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "再审一下核心loop", "feishu");
        assert_eq!(b.ingest(&read_tool("a.rs"), t0), vec!["读取 a.rs"]);
        assert!(
            b.ingest(&read_tool("b.rs"), t0 + Duration::from_secs(1))
                .is_empty(),
            "second hop inside THINK_FLUSH must not spend a Feishu edit"
        );
        assert!(b
            .ingest(&read_tool("c.rs"), t0 + Duration::from_secs(2))
            .is_empty());
        let flushed = b.tick(t0 + THINK_FLUSH);
        assert_eq!(
            flushed,
            vec!["读取 c.rs"],
            "latest hop lands on the 12s tick: {flushed:?}"
        );
        let quiet = b.tick(t0 + THINK_FLUSH + HEARTBEAT);
        assert!(
            quiet.is_empty(),
            "paced tool must not be followed by 还在处理: {quiet:?}"
        );
    }

    #[test]
    fn feishu_spaced_hops_still_patch() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "再审一下核心loop", "feishu");
        assert_eq!(b.ingest(&read_tool("a.rs"), t0), vec!["读取 a.rs"]);
        assert_eq!(
            b.ingest(&read_tool("b.rs"), t0 + THINK_FLUSH),
            vec!["读取 b.rs"],
            "hops already a flush apart still patch immediately"
        );
    }

    #[test]
    fn feishu_keeps_chinese_think_under_tool_line() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "再审一下核心loop", "feishu");
        let chunk = "计划先读入口再改标题，不扩大范围。".repeat(3);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let first = b.ingest(&read_tool("turn.rs"), t0 + Duration::from_secs(1));
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].starts_with("读取 turn.rs"), "{}", first[0]);
        assert!(first[0].contains("思考中"), "{}", first[0]);
        assert!(first[0].contains("计划先读入口"), "{}", first[0]);
        let later = b.tick(t0 + Duration::from_secs(1) + THINK_FLUSH);
        assert!(
            later.is_empty(),
            "same think+tool snapshot must not spend another edit: {later:?}"
        );
    }

    #[test]
    fn feishu_english_cot_after_tool_does_not_spend_an_edit() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "再审一下核心loop", "feishu");
        assert_eq!(b.ingest(&read_tool("a.rs"), t0), vec!["读取 a.rs"]);
        let chunk = "The user wants me to re-audit the core loop.".repeat(3);
        assert!(b
            .ingest(&reasoning(&chunk), t0 + Duration::from_secs(1))
            .is_empty());
        let later = b.tick(t0 + THINK_FLUSH);
        assert!(
            later.is_empty(),
            "Latin-majority CoT must not patch over the tool line: {later:?}"
        );
    }

    #[test]
    fn feishu_chinese_think_after_tool_lands_on_flush() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_channel(t0, "再审一下核心loop", "feishu");
        assert_eq!(b.ingest(&read_tool("turn.rs"), t0), vec!["读取 turn.rs"]);
        let chunk = "用户让我再审核心 loop，先看 drive 再给结论。".repeat(2);
        assert!(b
            .ingest(&reasoning(&chunk), t0 + Duration::from_secs(1))
            .is_empty());
        let later = b.tick(t0 + THINK_FLUSH);
        assert_eq!(later.len(), 1, "{later:?}");
        assert!(later[0].starts_with("读取 turn.rs"), "{}", later[0]);
        assert!(later[0].contains("思考中\n"), "{}", later[0]);
        assert!(later[0].contains("再审核心"), "{}", later[0]);
    }

    #[test]
    fn english_write_json_think_is_not_posted() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(t0, "write a file");
        let chunk = "I'll write the overnight marker file now.\n```json\n{\"name\": \"Write\", \"path\": \"a.txt\", \"contents\": \"R97_OK\\n\"}\n```\n";
        assert!(b.ingest(&reasoning(chunk), t0).is_empty());
        let lines = b.ingest(&reasoning(" next."), t0 + THINK_FLUSH);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Thinking\n"), "{}", lines[0]);
        assert!(lines[0].contains("I'll write the overnight marker"));
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("R97_OK") || l.contains("```")),
            "leaked Write JSON must not land in IM think: {lines:?}"
        );
    }

    #[test]
    fn think_tail_keeps_the_end() {
        let s = "aaa\nbbb\nccc";
        assert_eq!(think_tail(s, 200), s);
        let long = "x".repeat(80);
        let t = think_tail(&long, 10);
        assert!(t.starts_with('…'), "{t}");
        assert!(t.ends_with('x'));
    }

    #[test]
    fn content_delta_is_not_progress() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::new(t0);
        let ev = SessionEvent::Delta(DeltaEvent {
            channel: DeltaChannel::Content,
            text: "最终答案片段".into(),
            delta: true,
            reset: false,
            content_only: false,
        });
        assert!(b.ingest(&ev, t0).is_empty());
    }

    #[test]
    fn locale_follows_inbound_script() {
        assert_eq!(ImLocale::detect("帮我改标题"), ImLocale::Zh);
        assert_eq!(ImLocale::detect("fix the title in Chat.tsx"), ImLocale::En);
        assert_eq!(ImLocale::Zh.ack(), ACK_TEXT);
        assert!(ImLocale::En.ack().starts_with("Got it"));
        let t0 = Instant::now();
        let mut en = ProgressBuf::for_user(t0, "keep going on the refactor");
        let chunk = "I will read the entry then edit the title.".repeat(3);
        assert!(en.ingest(&reasoning(&chunk), t0).is_empty());
        let lines = en.ingest(&reasoning(" next."), t0 + THINK_FLUSH);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Thinking\n"), "{}", lines[0]);
        let mut beat = ProgressBuf::for_user(t0, "still going");
        assert!(beat.tick(t0 + Duration::from_secs(4)).is_empty());
        assert_eq!(beat.tick(t0 + HEARTBEAT), vec!["Still working…"]);
        assert!(beat
            .tick(t0 + HEARTBEAT + Duration::from_secs(4))
            .is_empty());
        assert_eq!(
            beat.tick(t0 + HEARTBEAT + HEARTBEAT),
            vec!["Still working…"]
        );
    }

    #[test]
    fn qq_wechat_typing_skips_text_heartbeat() {
        assert!(typing_covers_quiet("qq"));
        assert!(typing_covers_quiet("WeChat"));
        assert!(!typing_covers_quiet("webhook"));
        assert!(!typing_covers_quiet("telegram"));
        assert!(!typing_covers_quiet("dingtalk"));
        let t0 = Instant::now();
        let mut qq = ProgressBuf::for_channel(t0, "帮我改标题", "qq");
        assert!(qq.tick(t0 + HEARTBEAT).is_empty());
        assert!(qq.tick(t0 + HEARTBEAT + HEARTBEAT).is_empty());
        let mut wx = ProgressBuf::for_channel(t0, "帮我改标题", "wechat");
        assert!(wx.tick(t0 + HEARTBEAT).is_empty());
        let mut hook = ProgressBuf::for_channel(t0, "帮我改标题", "webhook");
        assert_eq!(hook.tick(t0 + HEARTBEAT), vec![HEARTBEAT_TEXT]);
        let chunk = "计划先读入口再改标题。".repeat(4);
        assert!(qq.ingest(&reasoning(&chunk), t0).is_empty());
        let think = qq.ingest(&reasoning("继续。"), t0 + THINK_FLUSH);
        assert_eq!(think.len(), 1, "{think:?}");
        assert!(think[0].starts_with("思考中\n"), "{}", think[0]);
    }

    fn read_tool(path: &str) -> SessionEvent {
        SessionEvent::assistant_usage(
            String::new(),
            String::new(),
            Some(vec![OpenAiToolCall::function(
                "c1",
                "Read",
                json!({"path": path}).to_string(),
            )]),
            0,
            0,
            None,
            None,
        )
    }

    #[test]
    fn stack_progress_is_append_only_chats() {
        assert!(stack_progress("qq"));
        assert!(stack_progress("WeChat"));
        assert!(stack_progress("dingtalk"));
        assert!(!stack_progress("telegram"));
        assert!(!stack_progress("webhook"));
        assert!(!stack_progress("feishu"));
        assert!(!stack_progress("wecom"));
    }

    #[test]
    fn edit_channel_finish_does_not_post_leftover_think() {
        let t0 = Instant::now();
        let chunk = "计划先读入口再改标题。".repeat(4);
        for ch in ["feishu", "telegram", "wecom", "webhook"] {
            let mut b = ProgressBuf::for_channel(t0, "帮我改标题", ch);
            assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
            let late = b.finish(t0 + THINK_FLUSH);
            assert!(
                late.is_empty(),
                "{ch} finish must not overwrite the answer: {late:?}"
            );
        }
        let mut qq = ProgressBuf::for_channel(t0, "帮我改标题", "qq");
        qq.ingest(&reasoning(&chunk), t0);
        let qq_done = qq.finish(t0 + THINK_FLUSH);
        assert_eq!(qq_done.len(), 1, "{qq_done:?}");
        assert!(qq_done[0].starts_with("思考中\n"), "{}", qq_done[0]);
    }

    #[test]
    fn qq_coalesces_think_and_first_tool() {
        let t0 = Instant::now();
        let mut qq = ProgressBuf::for_channel(t0, "帮我改标题", "qq");
        let chunk = "计划先读入口再改标题。".repeat(4);
        assert!(qq.ingest(&reasoning(&chunk), t0).is_empty());
        let lines = qq.ingest(&read_tool("src/main.rs"), t0 + Duration::from_secs(2));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("计划先读"), "{}", lines[0]);
        assert!(lines[0].contains("读取 src/main.rs"), "{}", lines[0]);
    }

    #[test]
    fn qq_holds_later_tools_until_flush_or_finish() {
        let t0 = Instant::now();
        let mut qq = ProgressBuf::for_channel(t0, "帮我改标题", "qq");
        assert_eq!(qq.ingest(&read_tool("a.rs"), t0), vec!["读取 a.rs"]);
        let held = qq.ingest(&read_tool("b.rs"), t0 + Duration::from_secs(1));
        assert!(held.is_empty(), "second hop must wait: {held:?}");
        let flushed = qq.tick(t0 + THINK_FLUSH);
        assert_eq!(flushed.len(), 1, "{flushed:?}");
        assert!(flushed[0].contains("读取 b.rs"), "{}", flushed[0]);

        let mut wx = ProgressBuf::for_channel(t0, "帮我改标题", "wechat");
        assert_eq!(wx.ingest(&read_tool("a.rs"), t0), vec!["读取 a.rs"]);
        assert!(wx
            .ingest(&read_tool("b.rs"), t0 + Duration::from_secs(1))
            .is_empty());
        let done = wx.finish(t0 + Duration::from_secs(2));
        assert_eq!(done, vec!["读取 b.rs"], "{done:?}");
    }

    #[test]
    fn dingtalk_still_heartbeats_when_idle() {
        let t0 = Instant::now();
        let mut dt = ProgressBuf::for_channel(t0, "帮我改标题", "dingtalk");
        assert_eq!(dt.tick(t0 + HEARTBEAT), vec![HEARTBEAT_TEXT]);
        let mut hook = ProgressBuf::for_channel(t0, "帮我改标题", "webhook");
        let first = hook.ingest(&read_tool("a.rs"), t0);
        let second = hook.ingest(&read_tool("b.rs"), t0 + Duration::from_secs(1));
        assert_eq!(first, vec!["读取 a.rs"]);
        assert_eq!(second, vec!["读取 b.rs"]);
    }

    fn write_tool(path: &str) -> SessionEvent {
        SessionEvent::assistant_usage(
            String::new(),
            String::new(),
            Some(vec![OpenAiToolCall::function(
                "w1",
                "Write",
                json!({"path": path, "content": "x"}).to_string(),
            )]),
            0,
            0,
            None,
            None,
        )
    }

    fn shell_tool(cmd: &str) -> SessionEvent {
        SessionEvent::assistant_usage(
            String::new(),
            String::new(),
            Some(vec![OpenAiToolCall::function(
                "s1",
                "Shell",
                json!({"command": cmd}).to_string(),
            )]),
            0,
            0,
            None,
            None,
        )
    }

    #[test]
    fn webhook_posts_next_hop_tool_without_waiting_gap() {
        let t0 = Instant::now();
        let mut hook = ProgressBuf::for_channel(t0, "写个脚本", "webhook");
        assert_eq!(
            hook.ingest(&write_tool(".grok-hyper/overnight/r61.py"), t0),
            vec!["写入 .grok-hyper/overnight/r61.py"]
        );
        assert_eq!(
            hook.ingest(
                &shell_tool("python3 .grok-hyper/overnight/r61.py"),
                t0 + Duration::from_secs(1)
            ),
            vec!["命令 python3 .grok-hyper/overnight/r61.py"]
        );
    }

    #[test]
    fn english_stub_is_skipped_after_a_tool_line() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(t0, "写个脚本");
        assert_eq!(
            b.ingest(&write_tool(".grok-hyper/overnight/im_live.py"), t0),
            vec!["写入 .grok-hyper/overnight/im_live.py"]
        );
        let chunk = "The user wants me to write the file then run it.".repeat(3);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let later = b.ingest(&reasoning(" still going."), t0 + THINK_FLUSH);
        assert!(
            later.is_empty(),
            "empty 思考中 after a tool line is think waste: {later:?}"
        );
    }

    #[test]
    fn english_stub_is_skipped_when_tools_flush_think() {
        let t0 = Instant::now();
        let mut b = ProgressBuf::for_user(t0, "写个脚本");
        let chunk = "The user wants me to write a file then run it with python.".repeat(2);
        assert!(b.ingest(&reasoning(&chunk), t0).is_empty());
        let lines = b.ingest(&write_tool(".grok-hyper/overnight/im_live.py"), t0);
        assert_eq!(
            lines,
            vec!["写入 .grok-hyper/overnight/im_live.py"],
            "tool hop must not prepend empty 思考中: {lines:?}"
        );
    }
}
