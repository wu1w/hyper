//! Hidden cards injected at the start of a user turn (skills, web, locate, numeric, doc-read).

use super::{Agent, Completer};
use crate::mcp::card_for as mcp_card;
use crate::memory::card_for as memory_card;
use crate::permit::PLAN_CARD;
use crate::skills::{hidden_card, match_tool_output, match_user};
use crate::sticky;

impl<C: Completer> Agent<C> {
    pub(crate) fn inject_notes(
        &mut self,
        user: &str,
        forced_skill: Option<&str>,
        forced_mcp: Option<&str>,
    ) {
        if self.cursor_wire() {
            self.inject_cursor_mode_notes(user, forced_skill, forced_mcp);
            return;
        }
        if !sticky::live_has_memory_note(&self.messages) {
            if let Some(store) = &self.memory {
                if let Some(md) = store.read_memory_md() {
                    if let Some(card) = memory_card(user, &md) {
                        self.push_hidden_user(card);
                    }
                }
            }
        }
        if forced_skill.is_some() {
            let stubbed = sticky::stub_live_skill_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if !sticky::live_has_skill_note(&self.messages) {
            let skill = forced_skill
                .and_then(|n| self.skills.get(n))
                .or_else(|| match_user(&self.skills, user));
            if let Some(sk) = skill {
                match hidden_card(sk) {
                    Some(card) => self.push_hidden_user(card),
                    None => self.note(&format!(
                        "hyper: skill {} over {} tok; not injected",
                        sk.name,
                        sticky::SKILL_BODY_MAX_TOKENS
                    )),
                }
            }
        }
        if forced_mcp.is_some() {
            let stubbed = sticky::stub_live_mcp_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if crate::tools_schema::has_tool(&self.tools, "mcp")
            && !sticky::live_has_mcp_note(&self.messages)
        {
            if let Some(card) = mcp_card(&self.mcp, user, forced_mcp) {
                self.push_hidden_user(card);
            }
        }
        if self.plan_mode && !sticky::live_has_plan_note(&self.messages) {
            self.push_hidden_user(PLAN_CARD);
        }
        if (self.plan_mode || self.clarify_mode) && !sticky::live_has_clarify_note(&self.messages) {
            self.push_hidden_user(crate::clarify::CLARIFY_CARD);
        }
        if self.narrate && !sticky::live_has_style_note(&self.messages) {
            self.push_hidden_user(sticky::STYLE_CARD);
        }
        if crate::cron::wants_cron_card(user) && !sticky::live_has_cron_note(&self.messages) {
            self.push_hidden_user(crate::cron::CRON_CARD);
        }
        self.inject_out_dir(user);
        self.inject_doc_read(user);
    }

    /// Cursor/Responses: only plan/clarify mode cards and an explicit
    /// `[skill:]` / `[mcp:]` prefix. No 27B locate/out/style/memory lectures.
    fn inject_cursor_mode_notes(
        &mut self,
        _user: &str,
        forced_skill: Option<&str>,
        forced_mcp: Option<&str>,
    ) {
        if forced_skill.is_some() {
            let stubbed = sticky::stub_live_skill_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if !sticky::live_has_skill_note(&self.messages) {
            if let Some(sk) = forced_skill.and_then(|n| self.skills.get(n)) {
                match hidden_card(sk) {
                    Some(card) => self.push_hidden_user(card),
                    None => self.note(&format!(
                        "hyper: skill {} over {} tok; not injected",
                        sk.name,
                        sticky::SKILL_BODY_MAX_TOKENS
                    )),
                }
            }
        }
        if forced_mcp.is_some() {
            let stubbed = sticky::stub_live_mcp_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if crate::tools_schema::has_tool(&self.tools, "mcp")
            && forced_mcp.is_some()
            && !sticky::live_has_mcp_note(&self.messages)
        {
            if let Some(card) = mcp_card(&self.mcp, _user, forced_mcp) {
                self.push_hidden_user(card);
            }
        }
        if self.plan_mode && !sticky::live_has_plan_note(&self.messages) {
            self.push_hidden_user(PLAN_CARD);
        }
        if (self.plan_mode || self.clarify_mode) && !sticky::live_has_clarify_note(&self.messages) {
            self.push_hidden_user(crate::clarify::CLARIFY_CARD);
        }
    }

    pub(crate) fn inject_out_dir(&mut self, user: &str) {
        if self.cursor_wire() {
            return;
        }
        if forbids_tools(user) {
            return;
        }
        if sticky::live_has_out_note(&self.messages) {
            let n = sticky::stub_live_out_notes(&mut self.messages);
            self.note_stubbed(n);
        }
        self.push_ephemeral_note(crate::out_dir::OUT_CARD);
    }

    pub(crate) fn inject_doc_read(&mut self, text: &str) {
        if self.cursor_wire() {
            return;
        }
        if forbids_tools(text) || sticky::live_has_doc_read_note(&self.messages) {
            return;
        }
        if crate::doc_read::wants_doc_read_card(text) {
            self.push_hidden_user(crate::doc_read::DOC_READ_CARD);
        }
    }

    pub(crate) fn inject_window_overlay_note(&mut self) {
        if self.cursor_wire() {
            self.window_overlay.take();
            return;
        }
        let Some(o) = self.window_overlay.take() else {
            return;
        };
        self.push_hidden_user(format!(
            "HYPER_WORKING_WINDOW={} overlays config.toml working_window={}. Live window is {}. Unset HYPER_WORKING_WINDOW to use the file. Page large files with read(path, offset, limit).",
            o.from_env, o.from_file, self.working_window
        ));
    }

    /// Local weights are weakest exactly on post-cutoff facts. When the task
    /// smells like fresh-world knowledge and `web` is armed, one short hidden
    /// fact names the tool — instead of a standing lecture in every prompt.
    pub(crate) fn inject_web_hint(&mut self, user: &str) {
        if self.cursor_wire() {
            return;
        }
        if !crate::tools_schema::has_tool(&self.tools, "web") {
            return;
        }
        if forbids_tools(user) || !wants_web_check(user) {
            return;
        }
        self.push_hidden_user(WEB_HINT);
    }

    /// Quantitative reasoning gets one short, task-local self-check cue. It is
    /// absent from ordinary chat and coding turns, and does not force another
    /// model hop: the model keeps control over when its answer is ready.
    pub(crate) fn inject_numeric_check_hint(&mut self, user: &str) {
        if self.cursor_wire() {
            return;
        }
        if wants_numeric_check(user) {
            self.push_hidden_user(NUMERIC_CHECK_HINT);
        }
    }

    pub(crate) fn inject_locate(&mut self, user: &str) {
        if self.cursor_wire() {
            return;
        }
        if forbids_tools(user) || !wants_auto_locate(user) {
            return;
        }
        let Some(idx) = &self.code_index else {
            return;
        };
        let Some(spans) = idx.render_query(user, None) else {
            return;
        };
        self.push_hidden_user(format!("[locate]\n{spans}"));
    }

    pub(crate) fn inject_skill_from_tools(&mut self, output: &str) {
        if self.cursor_wire() {
            return;
        }
        let Some(sk) = match_tool_output(&self.skills, output) else {
            return;
        };
        if sticky::live_has_skill_note(&self.messages) {
            let stubbed = sticky::stub_live_skill_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if let Some(card) = hidden_card(sk) {
            self.push_hidden_user(card);
        }
    }
}

/// One line, task-scoped, only when the trigger fires. Names the exact call
/// shape so a 27B does not have to invent it.
pub(crate) const WEB_HINT: &str = "[web] This question may need current facts; training memory can be stale. Search with WebSearch or fetch with WebFetch, then cite sources.";

/// One-line arithmetic hygiene for the small subset of prompts whose answer
/// depends on a derived probability, percentage or threshold. This stays out
/// of the frozen system prompt and asks for no extra prose or forced turn.
pub(crate) const NUMERIC_CHECK_HINT: &str = "[verify:numeric] If the conclusion includes a probability, percent, or threshold derived from the problem, substitute back once internally before you ship. Distinguish percent vs percentage points vs the requested variable; if it checks out, answer directly — do not add a recap section.";

pub(crate) fn wants_numeric_check(user: &str) -> bool {
    let lower = user.to_lowercase();
    const CODE_MARKS: &[&str] = &[
        "代码",
        "函数",
        "源码",
        "编译",
        "单测",
        "测试用例",
        "正则",
        ".py",
        ".rs",
        ".js",
        ".ts",
        " code",
        "function",
        "compile",
        "unit test",
        "regex",
        " bug",
    ];
    if CODE_MARKS.iter().any(|mark| lower.contains(mark)) || has_call_ident(user) {
        return false;
    }
    const QUANTITY_MARKS: &[&str] = &[
        "%",
        "％",
        "概率",
        "准确率",
        "百分",
        "百分点",
        "阈值",
        "临界",
        "期望值",
        "赔率",
        "比率",
        "比例",
        "方差",
        "置信区间",
        "probability",
        "accuracy",
        "percent",
        "percentage point",
        "threshold",
        "expected value",
        "odds",
        "variance",
        "confidence interval",
    ];
    const REASONING_MARKS: &[&str] = &[
        "求",
        "计算",
        "比较",
        "推导",
        "证明",
        "估计",
        "判断",
        "讨论",
        "边界",
        "阈值",
        "临界",
        "期望",
        "calculate",
        "compare",
        "derive",
        "prove",
        "estimate",
        "evaluate",
        "discuss",
        "boundary",
        "threshold",
        "expected",
    ];
    QUANTITY_MARKS.iter().any(|mark| lower.contains(mark))
        && REASONING_MARKS.iter().any(|mark| lower.contains(mark))
}

/// Freshness smell: explicit recency words, a 2025+ year, or a pasted URL.
/// Deliberately narrow — a false fire costs one useless hidden line, a missed
/// fire costs nothing the model did not already lack.
pub(crate) fn wants_web_check(user: &str) -> bool {
    if user.contains("不要联网") || user.contains("不要搜索") || user.contains("别联网")
    {
        return false;
    }
    if user.contains("http://") || user.contains("https://") {
        return true;
    }
    const MARKS: &[&str] = &[
        "最新",
        "近期",
        "最近发布",
        "今天",
        "今年",
        "现在的",
        "目前的",
        "新闻",
        "行情",
        "股价",
        "汇率",
        "多少钱",
        "价格是多少",
        "什么时候发布",
        "发布了吗",
        "上市了吗",
        "latest version",
        "latest release",
        "release date",
        "what's new in",
        "recent news",
        "price of",
    ];
    let lower = user.to_lowercase();
    if MARKS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    mentions_recent_year(user)
}

/// Any literal year 2025–2099 in the task text.
fn mentions_recent_year(user: &str) -> bool {
    let bytes = user.as_bytes();
    for w in bytes.windows(5) {
        if w[0] == b'2' && w[1] == b'0' && w[2].is_ascii_digit() && w[3].is_ascii_digit() {
            // Not part of a longer digit run (e.g. 202611 order ids).
            if w[4].is_ascii_digit() {
                continue;
            }
            let year = 2000 + u32::from(w[2] - b'0') * 10 + u32::from(w[3] - b'0');
            if (2025..=2099).contains(&year) {
                return true;
            }
        }
    }
    if bytes.len() >= 4 {
        let w = &bytes[bytes.len() - 4..];
        if w[0] == b'2' && w[1] == b'0' && w[2].is_ascii_digit() && w[3].is_ascii_digit() {
            let year = 2000 + u32::from(w[2] - b'0') * 10 + u32::from(w[3] - b'0');
            return (2025..=2099).contains(&year);
        }
    }
    false
}

pub(crate) fn forbids_tools(user: &str) -> bool {
    if user.contains("不要调用工具") || user.contains("不要用工具") || user.contains("不要开工具")
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    l.contains("don't use tools") || l.contains("do not use tools")
}

pub(crate) fn wants_auto_locate(user: &str) -> bool {
    if [
        "修",
        "实现",
        "定位",
        "在哪",
        "哪里",
        "缺陷",
        "崩溃",
        "bug",
        "立刻改",
        "必须改",
    ]
    .iter()
    .any(|p| user.contains(p))
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    if l.contains("fix ") || l.contains("implement ") {
        return true;
    }
    if has_call_ident(user) {
        return true;
    }
    user.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.')))
        .any(|t| {
            t.len() >= 3
                && (t.contains('_')
                    || t.contains('/')
                    || (t.contains('.') && !t.starts_with('.'))
                    || (t.chars().any(|c| c.is_ascii_uppercase())
                        && t.chars().any(|c| c.is_ascii_lowercase())))
        })
}

fn has_call_ident(user: &str) -> bool {
    let bytes = user.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'(' && i - start >= 3 {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}
