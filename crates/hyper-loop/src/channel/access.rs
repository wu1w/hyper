//! Sender / group gate. Wash of QwenPaw `dm_policy` / `group_policy` /
//! `allow_from` / `require_mention`. Unknown DMs get a pairing code.

use super::envelope::NativePayload;
use super::pair;
use super::ChannelEndpoint;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Deny(&'static str),
    /// Sender just bound this DM; do not run the pair code as a user turn.
    Paired,
}

pub fn admit(ep: &ChannelEndpoint, env: &NativePayload) -> GateDecision {
    let sender = env.sender_id.trim();
    if !sender.is_empty() && ep.deny_from.iter().any(|d| d == sender) {
        return GateDecision::Deny("deny_from");
    }
    let group = env.is_group();
    let mentioned = env.is_mentioned();
    let policy = if group {
        ep.group_policy.as_str()
    } else {
        ep.dm_policy.as_str()
    };
    match policy {
        "closed" | "disabled" => GateDecision::Deny("closed"),
        "allowlist" => allowlist(ep, env, sender, group),
        "mention" => {
            if group && !mentioned {
                GateDecision::Deny("mention")
            } else {
                allowlist_if_set(ep, sender)
            }
        }
        _ => {
            // open
            if ep.require_mention && group && !mentioned {
                return GateDecision::Deny("mention");
            }
            allowlist_if_set(ep, sender)
        }
    }
}

fn allowlist(ep: &ChannelEndpoint, env: &NativePayload, sender: &str, group: bool) -> GateDecision {
    if pair::is_allowed(ep, sender) {
        return GateDecision::Allow;
    }
    if !group && pair::try_pair(ep, sender, &env.query_text()) {
        return GateDecision::Paired;
    }
    GateDecision::Deny("allowlist")
}

fn allowlist_if_set(ep: &ChannelEndpoint, sender: &str) -> GateDecision {
    if ep.allow_from.is_empty() {
        return GateDecision::Allow;
    }
    if pair::is_allowed(ep, sender) {
        GateDecision::Allow
    } else {
        GateDecision::Deny("allowlist")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    fn env(group: bool, mentioned: bool, sender: &str) -> NativePayload {
        let mut p = NativePayload::text_only("telegram", "hi");
        p.sender_id = sender.into();
        let mut meta = Map::new();
        meta.insert("is_group".into(), json!(group));
        meta.insert("is_mentioned".into(), json!(mentioned));
        p.meta = meta;
        p
    }

    #[test]
    fn open_dm_allows() {
        let ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            dm_policy: "open".into(),
            group_policy: "open".into(),
            require_mention: false,
            ..ChannelEndpoint::default()
        };
        assert_eq!(admit(&ep, &env(false, false, "1")), GateDecision::Allow);
    }

    #[test]
    fn default_dm_is_allowlist() {
        let ep = ChannelEndpoint {
            id: format!("tg-{}", uuid::Uuid::new_v4().simple()),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        assert_eq!(ep.dm_policy, "allowlist");
        assert_eq!(ep.group_policy, "mention");
        assert!(ep.require_mention);
        assert_eq!(
            admit(&ep, &env(false, false, "1")),
            GateDecision::Deny("allowlist")
        );
        let mut group = env(true, false, "1");
        assert_eq!(admit(&ep, &group), GateDecision::Deny("mention"));
        group = env(true, true, "1");
        assert_eq!(admit(&ep, &group), GateDecision::Allow);
    }

    #[test]
    fn dm_pair_code_binds() {
        let ep = ChannelEndpoint {
            id: format!("tg-pair-{}", uuid::Uuid::new_v4().simple()),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        let first = env(false, false, "42");
        assert_eq!(admit(&ep, &first), GateDecision::Deny("allowlist"));
        let mut code_env = env(false, false, "42");
        code_env.text = super::super::pair::ensure_code(&ep);
        code_env.content_parts = vec![super::super::envelope::ContentPart::text(&code_env.text)];
        assert_eq!(admit(&ep, &code_env), GateDecision::Paired);
        assert_eq!(admit(&ep, &env(false, false, "42")), GateDecision::Allow);
        assert_eq!(
            admit(&ep, &env(false, false, "99")),
            GateDecision::Deny("allowlist")
        );
    }

    #[test]
    fn group_require_mention() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            require_mention: true,
            ..ChannelEndpoint::default()
        };
        ep.group_policy = "open".into();
        assert_eq!(
            admit(&ep, &env(true, false, "1")),
            GateDecision::Deny("mention")
        );
        assert_eq!(admit(&ep, &env(true, true, "1")), GateDecision::Allow);
    }

    #[test]
    fn allow_from_filters() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            allow_from: vec!["42".into()],
            ..ChannelEndpoint::default()
        };
        ep.dm_policy = "open".into();
        assert_eq!(
            admit(&ep, &env(false, false, "1")),
            GateDecision::Deny("allowlist")
        );
        assert_eq!(admit(&ep, &env(false, false, "42")), GateDecision::Allow);
    }
}
