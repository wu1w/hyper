//! Three-way Grok transport: grok login session, xAI API key, OpenAI-compat.
//!
//! Auth rails (OAuth session vs API key vs forwarding Bearer) are independent
//! of the model wire. Cursor / grok-4.6 speak Responses; Qwen/llama.cpp stay
//! on Chat Completions. Completers live in
//! `agent::{ResponsesCompleter, TransportCompleter}`. This module classifies
//! the endpoint and never logs tokens.

use std::fmt;

use crate::auth::{
    expired_err, explicit_api_key, login_err, probe_session, session_token, SessionProbe,
};
use crate::config::Config;
use crate::error::Result;
use crate::family::Family;

pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const SESSION_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrokTransport {
    Session,
    ApiKey,
    OpenAiCompat,
}

impl GrokTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::ApiKey => "api_key",
            Self::OpenAiCompat => "openai_compat",
        }
    }
}

impl fmt::Display for GrokTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Model-facing HTTP shape. Auth headers still follow [`GrokTransport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFormat {
    Responses,
    ChatCompletions,
}

impl WireFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
        }
    }
}

impl fmt::Display for WireFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resolved endpoint. `Debug` redacts the bearer.
pub struct ResolvedTransport {
    pub mode: GrokTransport,
    pub base_url: String,
    token: String,
}

impl ResolvedTransport {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for ResolvedTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedTransport")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Custom OpenAI-compat URL: non-empty and not an xAI / cli-chat-proxy host.
pub fn custom_compat_url(base_url: &str) -> Option<&str> {
    let u = base_url.trim();
    if u.is_empty() {
        return None;
    }
    if is_builtin_xai_host(u) {
        return None;
    }
    Some(u.trim_end_matches('/'))
}

pub fn is_builtin_xai_host(base_url: &str) -> bool {
    let u = base_url.to_ascii_lowercase();
    u.contains("api.x.ai") || u.contains("cli-chat-proxy")
}

/// Priority: custom `base_url` → cli-chat-proxy session host → explicit key → grok login session.
pub fn resolve(cfg: &Config) -> Result<ResolvedTransport> {
    resolve_parts(cfg, explicit_api_key(cfg), probe_session(), None)
}

/// Same as [`resolve`], but refreshes an expired OAuth session when a refresh token exists.
pub async fn resolve_live(cfg: &Config) -> Result<ResolvedTransport> {
    match resolve(cfg) {
        Ok(r) => Ok(r),
        Err(e) => {
            if is_session_host(&cfg.server.base_url)
                || custom_compat_url(&cfg.server.base_url).is_none()
            {
                if explicit_api_key(cfg).is_none() || is_session_host(&cfg.server.base_url) {
                    let tok = crate::oauth::ensure_fresh_session().await?;
                    return resolve_parts(cfg, None, SessionProbe::Valid, Some(tok));
                }
            }
            Err(e)
        }
    }
}

pub fn is_session_host(base_url: &str) -> bool {
    base_url.to_ascii_lowercase().contains("cli-chat-proxy")
}

/// Headers the cli-chat-proxy expects for a grok login session token.
/// Forwarding / API-key rails stay Bearer-only (Cursor Responses body, no OAuth headers).
pub fn apply_grok_headers(
    mut req: reqwest::RequestBuilder,
    mode: GrokTransport,
) -> reqwest::RequestBuilder {
    if mode == GrokTransport::Session {
        req = req
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-grok-client-version", "1.0.0")
            .header("x-grok-client-identifier", "grok-hyper")
            .header("x-grok-client-mode", "console");
    }
    req
}

/// Sync heuristic: OAuth + API key always Responses. Custom forwarding uses
/// Responses when the configured family/model looks like Grok (Cursor wire).
pub fn prefers_responses(cfg: &Config) -> bool {
    match resolve(cfg) {
        Ok(r) => prefers_responses_for(cfg, &r),
        Err(_) => false,
    }
}

pub fn prefers_responses_for(cfg: &Config, resolved: &ResolvedTransport) -> bool {
    match resolved.mode {
        GrokTransport::Session | GrokTransport::ApiKey => true,
        GrokTransport::OpenAiCompat => grok_like_forwarding(cfg),
    }
}

fn forwarding_is_qwen(cfg: &Config) -> bool {
    matches!(
        cfg.server.family,
        Family::Qwen35 | Family::Qwen36 | Family::Qwen38
    ) || matches!(
        Family::detect(cfg.server.model.trim()),
        Some(Family::Qwen35 | Family::Qwen36 | Family::Qwen38)
    )
}

fn grok_like_forwarding(cfg: &Config) -> bool {
    // Do not trust default family=grok46 on a llama.cpp URL. Model id (g46-xhigh,
    // grok-4.6, …) is the sync signal; unknown custom URLs probe `/responses`.
    if forwarding_is_qwen(cfg) {
        return false;
    }
    Family::detect(cfg.server.model.trim()) == Some(Family::Grok46)
}

/// Live choice for [`crate::agent::TransportCompleter`]. Grok-like endpoints
/// skip the probe. Qwen forwarding stays on Chat Completions without probing
/// `/responses` (llama.cpp 404s slowly). Unknown custom URLs POST `/responses`
/// once: 404 → Chat.
pub async fn detect_wire(cfg: &Config, resolved: &ResolvedTransport) -> WireFormat {
    if prefers_responses_for(cfg, resolved) {
        return WireFormat::Responses;
    }
    if forwarding_is_qwen(cfg) {
        return WireFormat::ChatCompletions;
    }
    if resolved.mode != GrokTransport::OpenAiCompat {
        return WireFormat::Responses;
    }
    if probe_responses_path(resolved).await {
        WireFormat::Responses
    } else {
        WireFormat::ChatCompletions
    }
}

/// True when POST `/v1/responses` exists (400/401/422 count). 404 does not.
pub async fn probe_responses_path(resolved: &ResolvedTransport) -> bool {
    let url = format!("{}/responses", resolved.base_url.trim_end_matches('/'));
    let client = match crate::llm_http::build_client_for(2, 8, &resolved.base_url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Invalid on purpose: we only care whether the path exists.
    let body = serde_json::json!({
        "store": false,
        "max_output_tokens": 1,
        "input": []
    });
    let mut req = client.post(&url).json(&body);
    if !resolved.token().is_empty() {
        req = req.bearer_auth(resolved.token());
    }
    req = apply_grok_headers(req, resolved.mode);
    match req.send().await {
        Ok(resp) => resp.status().as_u16() != 404,
        Err(_) => false,
    }
}

/// Truncate an HTTP error body for logs. Never include the caller-supplied token.
pub fn http_error_snippet(status: reqwest::StatusCode, body: &str) -> String {
    let t = body.trim();
    let mut end = t.len().min(400);
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    let t = &t[..end];
    if t.is_empty() {
        status.to_string()
    } else {
        format!("{status}: {t}")
    }
}

pub(crate) fn resolve_parts(
    cfg: &Config,
    key: Option<String>,
    session: SessionProbe,
    session_tok: Option<String>,
) -> Result<ResolvedTransport> {
    if let Some(url) = custom_compat_url(&cfg.server.base_url) {
        return Ok(ResolvedTransport {
            mode: GrokTransport::OpenAiCompat,
            base_url: url.to_string(),
            token: cfg.server.api_key.clone(),
        });
    }
    // The 模型 card "grok login 会话" writes cli-chat-proxy. That rail wins even
    // if an API key is also saved, otherwise OAuth login is silently ignored.
    if is_session_host(&cfg.server.base_url) {
        return session_transport(session, session_tok);
    }
    if let Some(key) = key {
        return Ok(ResolvedTransport {
            mode: GrokTransport::ApiKey,
            base_url: XAI_BASE_URL.into(),
            token: key,
        });
    }
    session_transport(session, session_tok)
}

fn session_transport(
    session: SessionProbe,
    session_tok: Option<String>,
) -> Result<ResolvedTransport> {
    match session {
        SessionProbe::Valid => {
            let token = match session_tok {
                Some(t) => t,
                None => session_token()?,
            };
            Ok(ResolvedTransport {
                mode: GrokTransport::Session,
                base_url: SESSION_BASE_URL.into(),
                token,
            })
        }
        SessionProbe::Expired => Err(expired_err()),
        SessionProbe::Absent => Err(login_err()),
    }
}

/// Official compact POST target when this session is on Responses.
/// Forwarding Grok proxies often 404 `/responses/compact`; the caller falls back.
pub fn compact_creds(cfg: &Config) -> Option<(String, String)> {
    match resolve(cfg) {
        Ok(r)
            if prefers_responses_for(cfg, &r)
                && matches!(r.mode, GrokTransport::Session | GrokTransport::ApiKey) =>
        {
            Some((r.base_url.clone(), r.token().to_string()))
        }
        _ => None,
    }
}

/// UI-safe snapshot. Never includes a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicAuth {
    pub mode: &'static str,
    pub wire: &'static str,
    pub logged_in: bool,
    pub session: &'static str,
    pub api_key_set: bool,
}

pub fn public_auth(cfg: &Config) -> PublicAuth {
    let probe = probe_session();
    let session = match probe {
        SessionProbe::Valid => "valid",
        SessionProbe::Expired => "expired",
        SessionProbe::Absent => "absent",
    };
    let api_key_set = explicit_api_key(cfg).is_some();
    let (mode, wire) = match resolve(cfg) {
        Ok(r) => (
            r.mode.as_str(),
            if prefers_responses_for(cfg, &r) {
                WireFormat::Responses.as_str()
            } else {
                WireFormat::ChatCompletions.as_str()
            },
        ),
        Err(_) => ("none", WireFormat::ChatCompletions.as_str()),
    };
    PublicAuth {
        mode,
        wire,
        logged_in: probe == SessionProbe::Valid,
        session,
        api_key_set,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::is_placeholder_key;

    fn cfg(base: &str, key: &str) -> Config {
        let mut c = Config::default();
        c.server.base_url = base.into();
        c.server.api_key = key.into();
        c
    }

    #[test]
    fn custom_url_wins_over_key_and_session() {
        let c = cfg("http://127.0.0.1:8080/v1", "xai-should-not-win");
        let r = resolve_parts(
            &c,
            Some("xai-should-not-win".into()),
            SessionProbe::Valid,
            Some("sess".into()),
        )
        .unwrap();
        assert_eq!(r.mode, GrokTransport::OpenAiCompat);
        assert!(r.base_url.contains("127.0.0.1"));
    }

    #[test]
    fn builtin_xai_url_is_not_custom() {
        assert!(custom_compat_url("https://api.x.ai/v1").is_none());
        assert!(custom_compat_url("https://cli-chat-proxy.grok.com/v1").is_none());
        assert!(custom_compat_url("http://127.0.0.1:8080/v1").is_some());
        assert!(custom_compat_url("").is_none());
    }

    #[test]
    fn explicit_key_beats_session() {
        let c = cfg("", "xai-live-key");
        let r = resolve_parts(
            &c,
            Some("xai-live-key".into()),
            SessionProbe::Valid,
            Some("session-token".into()),
        )
        .unwrap();
        assert_eq!(r.mode, GrokTransport::ApiKey);
        assert_eq!(r.base_url, XAI_BASE_URL);
        assert_eq!(r.token(), "xai-live-key");
    }

    #[test]
    fn session_host_beats_saved_key() {
        let c = cfg(SESSION_BASE_URL, "xai-live-key");
        let r = resolve_parts(
            &c,
            Some("xai-live-key".into()),
            SessionProbe::Valid,
            Some("session-token".into()),
        )
        .unwrap();
        assert_eq!(r.mode, GrokTransport::Session);
        assert_eq!(r.base_url, SESSION_BASE_URL);
        assert_eq!(r.token(), "session-token");
    }

    #[test]
    fn placeholder_local_is_not_explicit_key() {
        let c = cfg("", "local");
        assert!(is_placeholder_key(&c.server.api_key));
        let r = resolve_parts(&c, None, SessionProbe::Valid, Some("session-token".into())).unwrap();
        assert_eq!(r.mode, GrokTransport::Session);
        assert_eq!(r.base_url, SESSION_BASE_URL);
        assert_eq!(r.token(), "session-token");
    }

    #[test]
    fn session_when_no_key() {
        let c = cfg("", "");
        let r = resolve_parts(&c, None, SessionProbe::Valid, Some("sess".into())).unwrap();
        assert_eq!(r.mode, GrokTransport::Session);
    }

    #[test]
    fn none_tells_user_to_login() {
        let c = cfg("", "");
        let err = resolve_parts(&c, None, SessionProbe::Absent, None).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("grok login"), "{s}");
        assert!(!s.contains("sess"));
    }

    #[test]
    fn expired_session_is_distinct() {
        let c = cfg("", "");
        let err = resolve_parts(&c, None, SessionProbe::Expired, None).unwrap_err();
        assert!(err.to_string().contains("过期") || err.to_string().contains("expired"));
    }

    #[test]
    fn debug_redacts_token() {
        let c = cfg("", "super-secret-key-xyz");
        let r = resolve_parts(
            &c,
            Some("super-secret-key-xyz".into()),
            SessionProbe::Absent,
            None,
        )
        .unwrap();
        let d = format!("{r:?}");
        assert!(!d.contains("super-secret-key-xyz"), "{d}");
        assert!(d.contains("<redacted>"), "{d}");
    }

    #[test]
    fn forwarding_g46_alias_prefers_responses() {
        let mut c = cfg("https://grok.example.com/v1", "tok");
        c.server.model = "g46-xhigh".into();
        c.server.family = Family::Auto;
        let r = resolve(&c).unwrap();
        assert_eq!(r.mode, GrokTransport::OpenAiCompat);
        assert!(prefers_responses_for(&c, &r));
        assert!(compact_creds(&c).is_none());
        assert_eq!(public_auth(&c).wire, "responses");
    }

    #[test]
    fn forwarding_qwen_stays_on_chat() {
        let mut c = cfg("http://127.0.0.1:8080/v1", "local");
        c.server.model = "Qwen3.8-27B-UD-Q8".into();
        c.server.family = Family::Grok46;
        let r = resolve(&c).unwrap();
        assert!(!prefers_responses_for(&c, &r));
        assert!(compact_creds(&c).is_none());
        assert_eq!(public_auth(&c).wire, "chat_completions");
    }

    #[test]
    fn qwenthin_alias_stays_on_chat() {
        let mut c = cfg("http://127.0.0.1:8080/v1", "local");
        c.server.model = "QwenThin".into();
        c.server.family = Family::Qwen38;
        let r = resolve(&c).unwrap();
        assert!(!prefers_responses_for(&c, &r));
        assert_eq!(public_auth(&c).wire, "chat_completions");
    }

    #[test]
    fn session_and_api_key_are_responses() {
        let c = cfg(SESSION_BASE_URL, "");
        let r = resolve_parts(&c, None, SessionProbe::Valid, Some("sess".into())).unwrap();
        assert!(prefers_responses_for(&c, &r));
        let mut keyed = cfg("", "xai-live-key");
        keyed.server.model = "grok-4.6".into();
        let r = resolve_parts(
            &keyed,
            Some("xai-live-key".into()),
            SessionProbe::Absent,
            None,
        )
        .unwrap();
        assert_eq!(r.mode, GrokTransport::ApiKey);
        assert!(prefers_responses_for(&keyed, &r));
    }
}
