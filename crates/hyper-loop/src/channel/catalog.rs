//! QwenPaw-shaped channel catalog for the console (kinds, copy, fields, QR).

use serde::Serialize;
use serde_json::Value;

use super::KINDS;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub secret: bool,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub hint: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct KindSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub mark: &'static str,
    /// Official-ish tile color (icon only).
    pub color: &'static str,
    pub qr: bool,
    /// One endpoint per kind (webhook may repeat).
    pub once: bool,
    /// In-process listener (`hyper web` and `hyper --channels`).
    pub in_process: bool,
    pub fields: &'static [FieldSpec],
}

pub const CATALOG: &[KindSpec] = &[
    KindSpec {
        id: "dingtalk",
        name: "钉钉",
        blurb: "扫码创应用后进程内连钉钉 Stream",
        mark: "钉",
        color: "#0089FF",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "client_id",
                label: "Client ID",
                secret: false,
                hint: "即 AppKey",
            },
            FieldSpec {
                key: "client_secret",
                label: "Client Secret",
                secret: true,
                hint: "即 AppSecret",
            },
        ],
    },
    KindSpec {
        id: "feishu",
        name: "飞书",
        blurb: "扫码创应用后进程内连飞书长连接",
        mark: "飞",
        color: "#3370FF",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "app_id",
                label: "App ID",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "app_secret",
                label: "App Secret",
                secret: true,
                hint: "",
            },
            FieldSpec {
                key: "domain",
                label: "域名",
                secret: false,
                hint: "feishu 或 lark",
            },
        ],
    },
    KindSpec {
        id: "qq",
        name: "QQ",
        blurb: "扫码绑定后进程内连 QQ 官方网关",
        mark: "Q",
        color: "#12B7F5",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "app_id",
                label: "App ID",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "client_secret",
                label: "Client Secret",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "wechat",
        name: "微信",
        blurb: "iLink 扫码后进程内长轮询 getupdates",
        mark: "微",
        color: "#07C160",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "bot_token",
                label: "Bot token",
                secret: true,
                hint: "扫码后自动填入",
            },
            FieldSpec {
                key: "base_url",
                label: "iLink base_url",
                secret: false,
                hint: "默认 https://ilinkai.weixin.qq.com",
            },
        ],
    },
    KindSpec {
        id: "wecom",
        name: "企业微信",
        blurb: "扫码后进程内连企微 AI Bot 长连接",
        mark: "企",
        color: "#2B7BD6",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "bot_id",
                label: "Bot ID",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "secret",
                label: "Secret",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "telegram",
        name: "Telegram",
        blurb: "Bot API 长轮询，控制台进程内接听",
        mark: "TG",
        color: "#229ED9",
        qr: false,
        once: true,
        in_process: true,
        fields: &[FieldSpec {
            key: "bot_token",
            label: "Bot token",
            secret: true,
            hint: "BotFather 发给你的 token",
        }],
    },
    KindSpec {
        id: "webhook",
        name: "Webhook",
        blurb: "POST /inbound，控制台进程内接听",
        mark: "WH",
        color: "#615CED",
        qr: false,
        once: false,
        in_process: true,
        fields: &[],
    },
];

/// Kinds with an in-process listener (`hyper web` / `hyper --channels`).
pub const IN_PROCESS_HELP: &str = "telegram, webhook, qq, wechat, wecom, dingtalk, or feishu";

pub fn in_process_kind(kind: &str) -> bool {
    spec(kind).is_some_and(|s| s.in_process)
        || matches!(kind.to_ascii_lowercase().as_str(), "http" | "console")
}

pub fn spec(kind: &str) -> Option<&'static KindSpec> {
    let k = kind.to_ascii_lowercase();
    CATALOG.iter().find(|s| s.id == k)
}

pub fn supports_qr(kind: &str) -> bool {
    spec(kind).is_some_and(|s| s.qr)
}

/// Configured `[[channels.endpoints]]` kinds (not cli / sidecar / console).
pub fn endpoint_kind(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    if matches!(k.as_str(), "cli" | "sidecar" | "console" | "") {
        return false;
    }
    KINDS.iter().any(|x| x.eq_ignore_ascii_case(&k))
}

pub fn catalog_json() -> Value {
    serde_json::to_value(CATALOG).unwrap_or(Value::Array(vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_kinds_are_in_catalog() {
        for k in ["dingtalk", "feishu", "qq", "wechat", "wecom"] {
            assert!(supports_qr(k), "{k}");
            assert!(endpoint_kind(k), "{k}");
        }
        assert!(!supports_qr("telegram"));
        assert!(endpoint_kind("telegram"));
        assert!(endpoint_kind("webhook"));
        assert!(!endpoint_kind("cli"));
        assert!(!endpoint_kind("console"));
        for s in CATALOG {
            assert!(s.in_process, "catalog must not list unimplemented {}", s.id);
            assert!(
                IN_PROCESS_HELP.contains(s.id),
                "{} missing from IN_PROCESS_HELP",
                s.id
            );
        }
        assert!(in_process_kind("feishu"));
        assert!(in_process_kind("http"));
        assert!(!in_process_kind("discord"));
    }
}
