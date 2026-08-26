//! Working-window compact: soft/hard thresholds, local archive, official xAI compact.

use super::{
    over_hard_threshold, over_soft_threshold, should_compact_at_user_turn, Agent, Completer,
};
use crate::session::{compact_messages, derive_messages, plan_compact, SessionEvent};
use crate::template::{render, RenderOpts};
use crate::tokenize::count_tokens;
use crate::tools_schema::{has_recall, recall_tool};

impl<C: Completer> Agent<C> {
    pub fn official_compaction(&self) -> Option<&crate::session::OfficialCompaction> {
        self.official_compaction.as_ref()
    }

    pub(crate) async fn compact_if_needed(&mut self) -> Option<String> {
        if self.working_window == 0 {
            return None;
        }
        let n = self.prefix_tokens().unwrap_or(0) as u64;
        let _ = self.try_official_compact(n).await;
        if !self.over_soft_window() {
            return None;
        }
        for _ in 0..2 {
            if !self.apply_compact_pass() {
                break;
            }
            if !self.over_soft_window() {
                return None;
            }
        }
        if !self.over_hard_window() {
            return None;
        }
        let n = self.prefix_tokens().unwrap_or(0);
        Some(format!(
            "budget:context ({n} prefix + {} reserve > {} window)",
            self.generation_reserve, self.working_window
        ))
    }

    /// Archive previous turns at the start of a follow-up user message so a
    /// finished long turn is not replayed as a cold prefill.
    pub(crate) async fn compact_at_user_turn(&mut self) {
        if self.working_window == 0 {
            return;
        }
        let Some(n) = self.prefix_tokens() else {
            return;
        };
        if self.try_official_compact(n as u64).await {
            return;
        }
        if !should_compact_at_user_turn(
            n,
            self.generation_reserve,
            self.working_window,
            self.compact_ratio,
        ) {
            return;
        }
        let _ = self.apply_compact_pass();
    }

    /// POST `/v1/responses/compact` when this session is on xAI/cli-chat-proxy.
    /// On success stores the opaque item and still applies local archive compact
    /// so Chat Completions stays bounded until ResponsesCompleter consumes it.
    pub(crate) async fn try_official_compact(&mut self, prompt_tokens: u64) -> bool {
        let Some((base, key)) = self.xai_compact.clone() else {
            return false;
        };
        if !crate::session::should_official_compact(
            prompt_tokens,
            self.working_window,
            self.compact_ratio,
        ) {
            return false;
        }
        match crate::session::run_official_compact(&base, &key, &self.messages).await {
            Ok(item) => {
                self.note(&format!(
                    "[compact] official id={} blob={}",
                    item.id,
                    item.debug_blob()
                ));
                self.official_compaction = Some(item.clone());
                self.completer
                    .set_official_compaction(self.official_compaction.clone());
                let _ = self.apply_compact_pass();
                true
            }
            Err(_) => {
                if prompt_tokens > crate::session::PRICE_CLIFF_TOKENS {
                    self.note("[compact] official failed; local archive");
                    let _ = self.apply_compact_pass();
                    true
                } else {
                    false
                }
            }
        }
    }

    pub(crate) fn apply_compact_pass(&mut self) -> bool {
        if !self.try_compact() {
            return false;
        }
        let skip = self.messages.iter().filter(|m| m.role != "system").count();
        self.completer.set_compaction_skip(skip);
        self.after_compact();
        let tools = self.enable_recall();
        if tools {
            self.note("[compact] cache_invalidated=compact,tools");
        } else {
            self.note("[compact] cache_invalidated=compact");
        }
        true
    }

    pub(crate) fn over_soft_window(&self) -> bool {
        match self.prefix_tokens() {
            Some(n) => over_soft_threshold(
                n,
                self.generation_reserve,
                self.working_window,
                self.compact_ratio,
            ),
            None => false,
        }
    }

    pub(crate) fn over_hard_window(&self) -> bool {
        match self.prefix_tokens() {
            Some(n) => over_hard_threshold(n, self.generation_reserve, self.working_window),
            None => false,
        }
    }

    pub(crate) fn prefix_tokens(&self) -> Option<u32> {
        let (family, kwargs) = self.completer.prefix_meter()?;
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(self.tools.as_slice())
        };
        let rendered = render(&RenderOpts {
            family,
            messages: &self.messages,
            tools,
            add_generation_prompt: true,
            kwargs,
        })
        .ok()?;
        count_tokens(family, &rendered.text).ok()
    }

    pub(crate) fn try_compact(&mut self) -> bool {
        if self.log.is_some() {
            let plan = self.log.as_ref().and_then(|log| plan_compact(log.events()));
            let Some(plan) = plan else {
                return false;
            };
            if let Some(mem) = &self.memory {
                let _ =
                    mem.write_compact_note(&self.session_id, plan.until_seq, &plan.archive_body());
            }
            self.log_event(SessionEvent::compact(plan));
            self.messages = derive_messages(self.log.as_ref().unwrap().events());
            true
        } else if let Some((_plan, msgs)) = compact_messages(&self.messages) {
            self.messages = msgs;
            crate::sticky::stub_expired_notes(&mut self.messages);
            true
        } else {
            false
        }
    }

    pub(crate) fn after_compact(&mut self) {
        self.handler.reset_repeat(&self.session_id);
        self.observed_paths.clear();
        if self.read_paths.is_empty() {
            return;
        }
        let mut paths: Vec<String> = self.read_paths.iter().cloned().collect();
        paths.sort();
        paths.truncate(16);
        let body = paths.join("\n");
        if !self.cursor_wire() {
            self.push_hidden_user(format!(
                "[compact] prior reads left the live window. Files still on disk — page with read(path, offset, limit); do not repeat the same unpaged read:\n{body}"
            ));
        }
        self.read_paths.clear();
    }

    /// Append `recall` after compact. Returns true when `tools[]` changed
    /// (`cache_invalidated=tools` on top of the compact miss).
    pub(crate) fn enable_recall(&mut self) -> bool {
        if self.tools.is_empty() || has_recall(&self.tools) {
            return false;
        }
        self.tools.push(recall_tool());
        true
    }
}
