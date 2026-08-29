//! SpaceXAI OAuth2 for grok login (`auth.x.ai`). Same rail as `grok login --oauth`
//! and `--device-auth`. Tokens go to `~/.grok/auth.json`. Never logged.

use std::net::SocketAddr;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::auth::{
    expired_err, login_err, persist_session_tokens, read_session_inner, SessionTokens,
};
use crate::error::{Error, Result};
use crate::CancelFlag;

/// Public Grok CLI OAuth client. xAI rejects unknown loopback clients.
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const OAUTH_HOST: &str = "127.0.0.1";
pub const OAUTH_PORT: u16 = 56121;
pub const REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(48));
    let hash = Sha256::digest(verifier.as_bytes());
    Pkce {
        verifier,
        challenge: URL_SAFE_NO_PAD.encode(hash),
    }
}

pub fn random_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(32))
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

pub fn authorize_url(pkce: &Pkce, state: &str, nonce: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?{}",
        form(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("nonce", nonce),
            ("plan", "generic"),
            ("referrer", "grok-hyper"),
        ])
    )
}

pub fn oauth_bind_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], OAUTH_PORT))
}

/// Loopback redirect registered for the Grok CLI client. Returns the auth code.
pub async fn wait_callback(expected_state: &str, cancel: &CancelFlag) -> Result<String> {
    let listener = bind_oauth_listener().await?;
    accept_oauth_code(listener, expected_state, cancel).await
}

pub async fn bind_oauth_listener() -> Result<TcpListener> {
    TcpListener::bind(oauth_bind_addr()).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            Error::msg(format!(
                "OAuth callback port {OAUTH_PORT} is in use. Use device-code login instead."
            ))
        } else {
            Error::msg(format!("could not bind OAuth callback: {e}"))
        }
    })
}

pub async fn accept_oauth_code(
    listener: TcpListener,
    expected_state: &str,
    cancel: &CancelFlag,
) -> Result<String> {
    wait_callback_on_listener(listener, expected_state, cancel).await
}

pub async fn wait_callback_on(
    addr: SocketAddr,
    expected_state: &str,
    cancel: &CancelFlag,
) -> Result<String> {
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            Error::msg(format!(
                "OAuth callback port {OAUTH_PORT} is in use. Use device-code login instead."
            ))
        } else {
            Error::msg(format!("could not bind OAuth callback: {e}"))
        }
    })?;
    wait_callback_on_listener(listener, expected_state, cancel).await
}

async fn wait_callback_on_listener(
    listener: TcpListener,
    expected_state: &str,
    cancel: &CancelFlag,
) -> Result<String> {
    let accept = async {
        let (mut sock, _) = listener
            .accept()
            .await
            .map_err(|e| Error::msg(e.to_string()))?;
        let mut buf = vec![0u8; 8192];
        let n = sock
            .read(&mut buf)
            .await
            .map_err(|e| Error::msg(e.to_string()))?;
        let req = String::from_utf8_lossy(&buf[..n]);
        let line = req.lines().next().unwrap_or("");
        let path = line.split_whitespace().nth(1).unwrap_or("");
        let qs = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(qs);
        let html_ok = callback_html("已登录 grok-hyper。可以关闭此窗口。");
        let html_err = callback_html("登录失败。可以关闭此窗口，返回控制台重试。");
        if let Some(err) = params.get("error") {
            let _ = sock.write_all(http_html(400, &html_err).as_bytes()).await;
            return Err(Error::msg(format!("oauth denied: {err}")));
        }
        let state = params.get("state").cloned().unwrap_or_default();
        if state != expected_state {
            let _ = sock.write_all(http_html(400, &html_err).as_bytes()).await;
            return Err(Error::msg("oauth state mismatch"));
        }
        let code = params.get("code").cloned().unwrap_or_default();
        if code.is_empty() {
            let _ = sock.write_all(http_html(400, &html_err).as_bytes()).await;
            return Err(Error::msg("oauth callback missing code"));
        }
        let _ = sock.write_all(http_html(200, &html_ok).as_bytes()).await;
        Ok(code)
    };
    tokio::select! {
        _ = cancel.cancelled() => Err(Error::msg("oauth cancelled")),
        _ = tokio::time::sleep(Duration::from_secs(300)) => Err(Error::msg("oauth timed out (5 min)")),
        out = accept => out,
    }
}

pub async fn exchange_code(code: &str, verifier: &str) -> Result<SessionTokens> {
    post_token(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .await
}

pub async fn refresh_tokens(refresh_token: &str) -> Result<SessionTokens> {
    post_token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ])
    .await
}

pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub interval_s: u64,
    pub expires_in_s: u64,
}

pub async fn request_device() -> Result<DeviceStart> {
    let v = post_form(&device_url(), &[("client_id", CLIENT_ID), ("scope", SCOPE)]).await?;
    let device_code = json_str(&v, "device_code")?;
    let user_code = json_str(&v, "user_code")?;
    let verification_uri = json_str(&v, "verification_uri")?;
    let complete = v
        .get("verification_uri_complete")
        .and_then(|x| x.as_str())
        .unwrap_or(&verification_uri)
        .to_string();
    let interval_s = v
        .get("interval")
        .and_then(|x| x.as_u64())
        .unwrap_or(5)
        .max(1);
    let expires_in_s = v
        .get("expires_in")
        .and_then(|x| x.as_u64())
        .unwrap_or(300)
        .max(30);
    Ok(DeviceStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: complete,
        interval_s,
        expires_in_s,
    })
}

pub enum DevicePoll {
    Pending,
    SlowDown,
    Tokens(SessionTokens),
    Denied(String),
}

pub async fn poll_device(device_code: &str) -> Result<DevicePoll> {
    let client = http_client()?;
    let body = form(&[
        ("grant_type", DEVICE_GRANT),
        ("device_code", device_code),
        ("client_id", CLIENT_ID),
    ]);
    let resp = client
        .post(token_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", "grok-hyper/0.1.0")
        .body(body)
        .send()
        .await
        .map_err(|e| Error::Http(redact_err(&e.to_string())))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(DevicePoll::Tokens(tokens_from_json(&parse_json_obj(
            &text,
        )?)?));
    }
    let err = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(|x| x.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("http {status}"));
    Ok(match err.as_str() {
        "authorization_pending" => DevicePoll::Pending,
        "slow_down" => DevicePoll::SlowDown,
        "access_denied" | "expired_token" => DevicePoll::Denied(err),
        other => DevicePoll::Denied(redact_err(other)),
    })
}

pub async fn poll_device_until(device: &DeviceStart, cancel: &CancelFlag) -> Result<SessionTokens> {
    let mut interval = Duration::from_secs(device.interval_s.max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in_s);
    loop {
        if cancel.is_cancelled() {
            return Err(Error::msg("oauth cancelled"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::msg("device-code login timed out"));
        }
        match poll_device(&device.device_code).await? {
            DevicePoll::Tokens(t) => return Ok(t),
            DevicePoll::Pending => {}
            DevicePoll::SlowDown => {
                interval += Duration::from_secs(5);
            }
            DevicePoll::Denied(e) => return Err(Error::msg(e)),
        }
        tokio::select! {
            _ = cancel.cancelled() => return Err(Error::msg("oauth cancelled")),
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Refresh an expired session in place. Returns a usable access token.
pub async fn ensure_fresh_session() -> Result<String> {
    match read_session_inner()? {
        Some(mat) if !mat.expired && !mat.token.is_empty() => Ok(mat.token),
        Some(mat) if !mat.refresh_token.is_empty() => {
            let tokens = refresh_tokens(&mat.refresh_token).await?;
            persist_session_tokens(&tokens)?;
            Ok(tokens.access_token)
        }
        Some(_) => Err(expired_err()),
        None => Err(login_err()),
    }
}

pub fn persist_oauth_tokens(tokens: SessionTokens) -> Result<()> {
    persist_session_tokens(&tokens)
}

pub fn open_system_browser(url: &str) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
}

async fn post_token(fields: &[(&str, &str)]) -> Result<SessionTokens> {
    let v = post_form(&token_url(), fields).await?;
    tokens_from_json(&v)
}

async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Value> {
    let client = http_client()?;
    let resp = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", "grok-hyper/0.1.0")
        .body(form(fields))
        .send()
        .await
        .map_err(|e| Error::Http(redact_err(&e.to_string())))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Http(format!(
            "oauth token {status}: {}",
            redact_err(&clip(&text, 180))
        )));
    }
    parse_json_obj(&text)
}

fn tokens_from_json(v: &Value) -> Result<SessionTokens> {
    let access = json_str(v, "access_token")?;
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(3600);
    Ok(SessionTokens {
        access_token: access,
        refresh_token: refresh,
        expires_at: now.saturating_add(expires_in),
    })
}

fn http_client() -> Result<reqwest::Client> {
    crate::llm_http::finish_client(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30)),
        &token_url(),
    )
}

fn token_url() -> String {
    std::env::var("HYPER_OAUTH_TOKEN_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| TOKEN_URL.into())
}

fn device_url() -> String {
    std::env::var("HYPER_OAUTH_DEVICE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEVICE_URL.into())
}

fn json_str(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::msg(format!("oauth response missing {key}")))
}

fn parse_json_obj(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|_| Error::msg("oauth response was not json"))
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_query(qs: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for part in qs.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_html(status: u16, body: &str) -> String {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn callback_html(msg: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>grok-hyper</title><body style=\"font-family:system-ui;padding:2rem;background:#111;color:#eee\"><p>{msg}</p></body>"
    )
}

fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        t.to_string()
    } else {
        t.chars().take(n).collect::<String>() + "…"
    }
}

fn redact_err(s: &str) -> String {
    let low = s.to_ascii_lowercase();
    if low.contains("eyj") || low.contains("access_token") || low.contains("refresh_token") {
        "oauth error (redacted)".into()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_has_pkce_and_client() {
        let pkce = generate_pkce();
        let url = authorize_url(&pkce, "st", "nn");
        assert!(url.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&pkce.challenge));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback"));
        assert!(url.contains("plan=generic"));
        assert!(
            !url.contains(&pkce.verifier),
            "verifier must not be in authorize URL"
        );
    }

    #[test]
    fn form_encodes_scope_spaces() {
        let f = form(&[("scope", "openid profile")]);
        assert_eq!(f, "scope=openid%20profile");
    }

    #[test]
    fn redact_drops_jwt_like_errors() {
        assert_eq!(
            redact_err("failed eyJhbGciOiJIUzI1NiJ9.aa.bb"),
            "oauth error (redacted)"
        );
        assert!(redact_err("authorization_pending").contains("pending"));
    }

    #[tokio::test]
    async fn callback_returns_code() {
        let cancel = CancelFlag::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let expected = "abcState";
        let server = tokio::spawn({
            let cancel = cancel.clone();
            async move { wait_callback_on(addr, expected, &cancel).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            b"GET /callback?code=the-code&state=abcState HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await
        .unwrap();
        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "the-code");
    }

    #[tokio::test]
    async fn exchange_code_hits_mock_token_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("grant_type=authorization_code"));
            assert!(req.contains("code=abc"));
            let body = r#"{"access_token":"tok-x","refresh_token":"ref-x","expires_in":3600}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        std::env::set_var("HYPER_OAUTH_TOKEN_URL", format!("http://{addr}/token"));
        let tokens = exchange_code("abc", "verifier").await.unwrap();
        std::env::remove_var("HYPER_OAUTH_TOKEN_URL");
        server.await.unwrap();
        assert_eq!(tokens.access_token, "tok-x");
        assert_eq!(tokens.refresh_token, "ref-x");
        assert!(tokens.expires_at > 1_000_000);
    }
}
