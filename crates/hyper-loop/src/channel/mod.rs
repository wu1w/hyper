//! Message channels. Wash of QwenPaw `app/channels`:
//! native payload → `content_parts` → per-session queue → send back.
//!
//! In-process listeners: webhook, telegram, QQ, WeChat, WeCom, Feishu, DingTalk.

mod access;
pub mod catalog;
mod chunk;
mod dingtalk;
mod envelope;
mod feishu;
pub(crate) mod ilink_cdn;
mod im_md;
mod inbound;
pub(crate) mod interaction;
mod mailbox;
mod manager;
mod outbound;
mod pair;
mod poll_lock;
mod progress;
mod qq;
pub mod qrcode;
mod router;
mod runtime;
mod telegram;
mod webhook;
mod wechat;
mod wecom;
pub(crate) mod xfer;

pub use catalog::{catalog_json, endpoint_kind, in_process_kind, CATALOG, IN_PROCESS_HELP};
pub use envelope::{ChannelAddress, ContentPart, NativePayload};
pub use inbound::{
    keep_client_watched, serve_adapter, serve_endpoint, serve_qq, spawn_im_pump, start_im_manager,
    ClientWatch,
};
pub use mailbox::{
    has_steer, push_steer, take_steer, BusyDecision, BusyPolicy, Inbound, Mailbox, SteerSlot,
};
pub use manager::{ChannelHandler, ChannelManager, IngestResult};
pub use outbound::{deliver, outbound_notification, parts_to_text, reply_parts, reply_text};
pub use qrcode::{fetch_qrcode, poll_qrcode};
pub use router::SessionRouter;
pub use runtime::run as run_channels;

/// QwenPaw leftover names still parse in old `config.toml`. Only
/// [`catalog::CATALOG`] kinds have an in-process adapter; Discord / Slack /
/// iMessage / … will not connect if enabled.
pub const KINDS: &[&str] = &[
    "cli",
    "sidecar",
    "console",
    "webhook",
    "telegram",
    "discord",
    "slack",
    "dingtalk",
    "feishu",
    "qq",
    "wechat",
    "wecom",
    "imessage",
    "matrix",
    "mattermost",
    "mqtt",
    "voice",
    "onebot",
    "sip",
    "xiaoyi",
    "yuanbao",
];

pub fn known_kind(kind: &str) -> bool {
    KINDS.iter().any(|k| k.eq_ignore_ascii_case(kind))
}

/// One configured inbound endpoint. QwenPaw `BaseChannelConfig` shape.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChannelEndpoint {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub deny_from: Vec<String>,
    #[serde(default)]
    pub require_mention: bool,
    /// `open` | `allowlist` | `closed`
    pub dm_policy: String,
    /// `open` | `allowlist` | `mention` | `closed`
    pub group_policy: String,
    /// HTTP POST for outbound `content_parts` (webhook / DingTalk sessionWebhook).
    pub reply_url: String,
    /// Listen address for `kind = "webhook"`.
    pub bind: String,
    pub secret: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, String>,
}

impl Default for ChannelEndpoint {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: String::new(),
            enabled: false,
            allow_from: Vec::new(),
            deny_from: Vec::new(),
            require_mention: true,
            dm_policy: "allowlist".into(),
            group_policy: "mention".into(),
            reply_url: String::new(),
            bind: String::new(),
            secret: String::new(),
            extra: std::collections::BTreeMap::new(),
        }
    }
}

fn extra_value_placeholder(v: &str) -> bool {
    v.is_empty() || v == "true" || v == "false" || v.starts_with("****")
}

/// 沉默 token 归一化：去掉方括号和中英文句读、合并空白、转大写。
/// `NO_REPLY。` / `[silent].` 也要能命中；整段语义不变（前后多一句
/// 仍不算沉默）。终稿拦截（manager::is_silence）和流式预览守卫
/// （progress::content_section）共用同一套归一化，两边不会分叉。
pub(crate) fn normalize_silence(text: &str) -> String {
    let stripped: String = text
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '[' | ']' | '。' | '．' | '.' | '!' | '！' | '~' | '…' | '?' | '？'
            )
        })
        .collect();
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

pub(crate) fn is_silence_token(norm: &str) -> bool {
    matches!(norm, "NO_REPLY" | "NO REPLY" | "SILENT")
}

/// 流式期间 token 逐字到达（"N" → "NO" → …）：任何沉默 token 的前缀都
/// 视为潜在的沉默，先压住不外播。空串（比如只到了一个 `[`）也压住。
pub(crate) fn is_silence_prefix(norm: &str) -> bool {
    norm.is_empty()
        || ["NO_REPLY", "NO REPLY", "SILENT"]
            .iter()
            .any(|tok| tok.starts_with(norm))
}

impl ChannelEndpoint {
    /// Console GET redacts `secret` and token extras. A round-trip save must
    /// not wipe the values still on disk.
    pub fn absorb_secrets_from(&mut self, prev: &ChannelEndpoint) {
        if self.secret.trim().is_empty() {
            self.secret = prev.secret.clone();
        }
        for (k, v) in &prev.extra {
            let incoming = self.extra.get(k).map(|s| s.as_str()).unwrap_or("");
            if extra_value_placeholder(incoming) {
                self.extra.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Keep prior secrets for endpoints the UI re-posts without them.
pub fn merge_channel_endpoints(
    old: &[ChannelEndpoint],
    incoming: Vec<ChannelEndpoint>,
) -> Vec<ChannelEndpoint> {
    incoming
        .into_iter()
        .map(|mut ep| {
            if !ep.id.is_empty() {
                if let Some(prev) = old.iter().find(|p| p.id == ep.id) {
                    ep.absorb_secrets_from(prev);
                }
            }
            ep
        })
        .collect()
}

pub fn upsert_channel_endpoint(list: &mut Vec<ChannelEndpoint>, mut add: ChannelEndpoint) {
    if add.id.trim().is_empty() {
        add.id = add.kind.clone();
    }
    if let Some(prev) = list.iter().find(|p| p.id == add.id) {
        add.absorb_secrets_from(prev);
    }
    if let Some(i) = list.iter().position(|p| p.id == add.id) {
        list[i] = add;
    } else {
        list.push(add);
    }
}

pub fn remove_channel_endpoint(list: &mut Vec<ChannelEndpoint>, id: &str) {
    list.retain(|e| e.id != id);
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Busy follow-up policy. `steer` is the responsive default; `/queue`
    /// remains available when the user explicitly wants a later turn.
    pub busy: String,
    /// Surfaces allowed to enqueue. `cli` and `sidecar` are always implied.
    pub enabled: Vec<String>,
    pub endpoints: Vec<ChannelEndpoint>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            busy: "steer".into(),
            enabled: vec!["cli".into(), "sidecar".into()],
            endpoints: Vec::new(),
        }
    }
}

impl ChannelsConfig {
    pub fn busy_policy(&self) -> BusyPolicy {
        self.busy.parse().unwrap_or(BusyPolicy::Steer)
    }

    pub fn list_json(&self) -> serde_json::Value {
        let mut rows = vec![
            serde_json::json!({"id":"cli","kind":"cli","enabled":true,"builtin":true}),
            serde_json::json!({"id":"sidecar","kind":"sidecar","enabled":true,"builtin":true}),
            serde_json::json!({"id":"console","kind":"console","enabled":self.enabled.iter().any(|s| s == "console"),"builtin":true}),
        ];
        for ep in &self.endpoints {
            rows.push(serde_json::json!({
                "id": ep.id,
                "kind": ep.kind,
                "enabled": ep.enabled,
                "bind": ep.bind,
                "require_mention": ep.require_mention,
                "dm_policy": ep.dm_policy,
                "group_policy": ep.group_policy,
                "builtin": false,
            }));
        }
        serde_json::json!(rows)
    }

    pub fn endpoint_for(&self, channel: &str) -> Option<&ChannelEndpoint> {
        if channel.is_empty() {
            return None;
        }
        self.endpoints.iter().find(|e| {
            e.enabled
                && (e.id.eq_ignore_ascii_case(channel) || e.kind.eq_ignore_ascii_case(channel))
        })
    }

    /// Resolve the exact adapter instance first, then fall back to the legacy
    /// platform kind. This keeps two bots of the same kind isolated.
    pub fn endpoint_for_payload(&self, env: &NativePayload) -> Option<&ChannelEndpoint> {
        env.meta
            .get("endpoint_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| self.endpoint_for(id))
            .or_else(|| self.endpoint_for(&env.channel))
    }
}

pub(crate) fn stamp_endpoint(env: &mut NativePayload, ep: &ChannelEndpoint) {
    env.meta.insert(
        "endpoint_id".into(),
        serde_json::Value::String(ep.id.clone()),
    );
    if env.channel.is_empty() {
        env.channel = if ep.kind.is_empty() {
            ep.id.clone()
        } else {
            ep.kind.clone()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_busy_is_steer() {
        assert_eq!(ChannelsConfig::default().busy_policy(), BusyPolicy::Steer);
        assert_eq!(
            ChannelsConfig {
                busy: "nope".into(),
                ..ChannelsConfig::default()
            }
            .busy_policy(),
            BusyPolicy::Steer
        );
    }

    #[test]
    fn payload_endpoint_id_beats_shared_platform_kind() {
        let cfg = ChannelsConfig {
            endpoints: vec![
                ChannelEndpoint {
                    id: "work-bot".into(),
                    kind: "feishu".into(),
                    enabled: true,
                    ..ChannelEndpoint::default()
                },
                ChannelEndpoint {
                    id: "personal-bot".into(),
                    kind: "feishu".into(),
                    enabled: true,
                    ..ChannelEndpoint::default()
                },
            ],
            ..ChannelsConfig::default()
        };
        let mut env = NativePayload::text_only("feishu", "hi");
        env.sender_id = "u1".into();
        stamp_endpoint(&mut env, &cfg.endpoints[1]);
        assert_eq!(
            cfg.endpoint_for_payload(&env).map(|ep| ep.id.as_str()),
            Some("personal-bot")
        );
        assert_eq!(env.route_key(), "personal-bot:dm:u1");
    }

    #[test]
    fn round_trip_save_keeps_secret_and_bot_token() {
        let mut prev = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            secret: "hook-secret".into(),
            ..ChannelEndpoint::default()
        };
        prev.extra.insert("bot_token".into(), "123:ABC".into());
        prev.extra.insert("bot_username".into(), "hyperbot".into());

        let mut incoming = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        incoming
            .extra
            .insert("bot_username".into(), "hyperbot".into());
        incoming.extra.insert("bot_token".into(), "true".into());

        let out = merge_channel_endpoints(&[prev], vec![incoming]);
        assert_eq!(out[0].secret, "hook-secret");
        assert_eq!(
            out[0].extra.get("bot_token").map(String::as_str),
            Some("123:ABC")
        );
        assert_eq!(
            out[0].extra.get("bot_username").map(String::as_str),
            Some("hyperbot")
        );
        assert!(out[0].enabled);
    }

    #[test]
    fn typed_secret_replaces() {
        let prev = ChannelEndpoint {
            id: "wh".into(),
            secret: "old".into(),
            ..ChannelEndpoint::default()
        };
        let incoming = ChannelEndpoint {
            id: "wh".into(),
            secret: "new".into(),
            ..ChannelEndpoint::default()
        };
        let out = merge_channel_endpoints(&[prev], vec![incoming]);
        assert_eq!(out[0].secret, "new");
    }

    #[test]
    fn upsert_does_not_drop_siblings() {
        let mut list = vec![ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            ..ChannelEndpoint::default()
        }];
        upsert_channel_endpoint(
            &mut list,
            ChannelEndpoint {
                id: "hook".into(),
                kind: "webhook".into(),
                ..ChannelEndpoint::default()
            },
        );
        assert_eq!(list.len(), 2);
        remove_channel_endpoint(&mut list, "hook");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "tg");
    }

    #[test]
    fn known_kind_includes_scan_platforms() {
        for k in ["feishu", "qq", "wechat", "wecom", "dingtalk"] {
            assert!(known_kind(k), "{k}");
            assert!(endpoint_kind(k), "{k}");
        }
        assert!(!endpoint_kind("cli"));
    }
}
