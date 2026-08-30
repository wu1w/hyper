//! Hidden cards at the start of a user turn: explicit `[skill:]` / `[mcp:]`,
//! plus plan/clarify mode cards and a compact `[workset]` snapshot.

use super::{Agent, Completer};
use crate::mcp::card_for as mcp_card;
use crate::permit::PLAN_CARD;
use crate::skills::hidden_card;
use crate::sticky;

impl<C: Completer> Agent<C> {
    pub(crate) fn inject_notes(
        &mut self,
        user: &str,
        forced_skill: Option<&str>,
        forced_mcp: Option<&str>,
    ) {
        self.inject_cursor_mode_notes(user, forced_skill, forced_mcp);
        self.inject_workset_note(user);
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
        if (crate::tools_schema::has_tool(&self.tools, "GetDynamicTools")
            || crate::tools_schema::has_tool(&self.tools, "mcp"))
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

    fn inject_workset_note(&mut self, user: &str) {
        self.refresh_workset_cards();
        self.inject_history_note(user);
        self.inject_im_note(user);
    }

    /// Stub live `[workset]` / `[rules]` and inject a fresh snapshot.
    /// Compact rebuilds from JSONL (which `derive_messages` stubs); without this
    /// the rest of the turn would only see `[workset] applied`.
    pub(crate) fn refresh_workset_cards(&mut self) {
        let stubbed = sticky::stub_live_workset_notes(&mut self.messages);
        self.note_stubbed(stubbed);
        let mut window: Vec<String> =
            super::dispatch::observed_from_messages(&self.messages, &self.workspace)
                .into_iter()
                .collect();
        for f in &self.editor_files {
            if !f.path.trim().is_empty() {
                window.push(f.path.clone());
            }
        }
        window.sort();
        if let Some(card) = super::workset::card(
            self.workspace.root(),
            &window,
            &self.editor_files,
            super::im_bridge_channel(&self.channel),
        ) {
            self.push_hidden_user(card);
        }
        if let Some(card) =
            super::workset::rules_card(self.workspace.root(), self.home.as_deref(), &window)
        {
            self.push_hidden_user(card);
        }
    }

    fn inject_history_note(&mut self, user: &str) {
        self.refresh_history_cards(user);
    }

    /// Stub live `[history]` and inject a fresh recap. Compact rebuilds from
    /// JSONL (which `derive_messages` stubs); without this the rest of the turn
    /// would only see `[history] applied`.
    pub(crate) fn refresh_history_cards(&mut self, user: &str) {
        if self.child.is_some() || self.print {
            return;
        }
        let stubbed = sticky::stub_live_history_notes(&mut self.messages);
        self.note_stubbed(stubbed);
        let Some(dir) = self
            .log
            .as_ref()
            .map(|l| l.dir().to_path_buf())
            .or_else(|| self.session_dir.clone())
        else {
            return;
        };
        let ws = self.workspace.root().display().to_string();
        let card = {
            let events = self.log.as_ref().map(|l| l.events());
            crate::session::history_card(
                &dir,
                &self.session_id,
                &ws,
                user,
                self.memory.as_ref(),
                events,
            )
        };
        if let Some(card) = card {
            self.push_hidden_user(card);
        }
    }

    fn inject_im_note(&mut self, user: &str) {
        if !super::im_bridge_channel(&self.channel) {
            return;
        }
        let stubbed = sticky::stub_live_im_notes(&mut self.messages);
        self.note_stubbed(stubbed);
        self.push_hidden_user(sticky::im_card(user));
    }

    pub(crate) fn inject_window_overlay_note(&mut self) {
        self.window_overlay.take();
    }
}

#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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

pub(crate) fn forbids_grep(user: &str) -> bool {
    if user.contains("不要 Grep")
        || user.contains("不要用 Grep")
        || user.contains("不要 grep")
        || user.contains("别 Grep")
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    l.contains("don't grep")
        || l.contains("do not grep")
        || l.contains("don't use grep")
        || l.contains("do not use grep")
        || phrase_at_word(l.as_str(), "no grep")
}

pub(crate) fn forbids_glob(user: &str) -> bool {
    if user.contains("不要 Glob")
        || user.contains("不要用 Glob")
        || user.contains("不要 glob")
        || user.contains("别 Glob")
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    l.contains("don't glob")
        || l.contains("do not glob")
        || l.contains("don't use glob")
        || l.contains("do not use glob")
        || phrase_at_word(l.as_str(), "no glob")
}

/// `needle` must not be a prefix of a longer ASCII word (`no glob` ≠ `no global`).
fn phrase_at_word(hay: &str, needle: &str) -> bool {
    let mut rest = hay;
    while let Some(i) = rest.find(needle) {
        let after = &rest[i + needle.len()..];
        if after
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric())
        {
            return true;
        }
        rest = &rest[i + 1..];
    }
    false
}

#[cfg(test)]
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

#[cfg(test)]
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
