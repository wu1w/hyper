//! Local credential resolution. Tokens never appear in `Display`, `Debug`,
//! logs, or UI JSON.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{Error, Result};

const PLACEHOLDERS: &[&str] = &[
    "local",
    "changeme",
    "placeholder",
    "your-api-key",
    "none",
    "dummy",
    "***",
    "sk-test",
];

/// True when `s` is empty or a dummy value such as llama.cpp `"local"`.
pub fn is_placeholder_key(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return true;
    }
    let low = t.to_ascii_lowercase();
    PLACEHOLDERS.iter().any(|p| low == *p)
        || low.starts_with("sk-test")
        || low.contains("your-api-key")
}

/// Config / env key that can actually authenticate (not `"local"`).
pub fn usable_key(s: &str) -> Option<&str> {
    let t = s.trim();
    if is_placeholder_key(t) {
        None
    } else {
        Some(t)
    }
}

pub fn grok_home() -> PathBuf {
    if let Ok(h) = std::env::var("GROK_HOME") {
        let h = h.trim();
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    crate::config::user_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
}

pub fn auth_json_path() -> PathBuf {
    grok_home().join("auth.json")
}

/// File exists. Does not read or validate the token.
pub fn session_file_present() -> bool {
    session_path_if_present().is_some()
}

fn session_path_if_present() -> Option<PathBuf> {
    let p = auth_json_path();
    if p.is_file() {
        return Some(p);
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionProbe {
    Absent,
    Valid,
    Expired,
}

/// Peek session file without returning the token.
pub fn probe_session() -> SessionProbe {
    match read_session_inner() {
        Ok(Some(mat)) => {
            if mat.expired {
                SessionProbe::Expired
            } else {
                SessionProbe::Valid
            }
        }
        Ok(None) | Err(_) => {
            if session_file_present() {
                SessionProbe::Expired
            } else {
                SessionProbe::Absent
            }
        }
    }
}

pub(crate) struct SessionMaterial {
    pub token: String,
    pub refresh_token: String,
    pub expired: bool,
}

/// Bearer for a grok login session. Errors mention `grok login` only — never
/// the file body or token.
pub fn session_token() -> Result<String> {
    match read_session_inner()? {
        Some(mat) if mat.expired => Err(expired_err()),
        Some(mat) if mat.token.is_empty() => Err(login_err()),
        Some(mat) => Ok(mat.token),
        None => Err(login_err()),
    }
}

pub fn login_err() -> Error {
    Error::msg(
        "没有 Grok 凭证。在「模型」里选 grok login → OAuth，或运行 `hyper login` / `grok login`，或设置 XAI_API_KEY。",
    )
}

pub fn expired_err() -> Error {
    Error::msg(
        "grok login 会话已过期。请在「模型」里重新 OAuth，或运行 `hyper login` / `grok login`。",
    )
}

pub(crate) fn read_session_inner() -> Result<Option<SessionMaterial>> {
    let Some(path) = session_path_if_present() else {
        return Ok(None);
    };
    let raw = fs::read_to_string(&path).map_err(|_| login_err())?;
    parse_auth_json(&raw, &path)
}

fn parse_auth_json(raw: &str, path: &Path) -> Result<Option<SessionMaterial>> {
    let v: Value = serde_json::from_str(raw)
        .map_err(|_| Error::msg("could not read grok login session; run `grok login`"))?;
    let Some(entry) = pick_session_entry(&v) else {
        return Ok(None);
    };
    let token = token_from_entry(entry).unwrap_or_default();
    if token.is_empty() {
        return Ok(None);
    }
    let refresh_token = first_string(entry, &["refresh_token"]).unwrap_or_default();
    let expired = entry_expired(entry, &token, path);
    Ok(Some(SessionMaterial {
        token,
        refresh_token,
        expired,
    }))
}

/// Grok CLI keys entries `{issuer}::{client_id}` with `key` as the bearer.
/// Older files use `https://accounts.x.ai/sign-in` or a flat `access_token`.
fn pick_session_entry(v: &Value) -> Option<&Value> {
    let obj = v.as_object()?;
    let mut ranked: Vec<&Value> = Vec::new();
    for (k, val) in obj {
        if k.contains("::") && val.is_object() {
            ranked.push(val);
        }
    }
    if let Some(legacy) = obj.get("https://accounts.x.ai/sign-in") {
        if legacy.is_object() {
            ranked.push(legacy);
        }
    }
    for val in obj.values() {
        if val.is_object()
            && token_from_entry(val).is_some()
            && !ranked.iter().any(|x| std::ptr::eq(*x, val))
        {
            ranked.push(val);
        }
    }
    ranked.push(v);
    ranked.into_iter().find(|e| token_from_entry(e).is_some())
}

fn token_from_entry(v: &Value) -> Option<String> {
    first_string(v, &["key", "access_token", "token"])
}

fn first_string(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn entry_expired(v: &Value, token: &str, path: &Path) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Some(exp) = parse_expiry(v.get("expires_at"))
        .or_else(|| parse_expiry(v.get("expiry")))
        .or_else(|| jwt_exp(token))
    {
        return now >= exp;
    }
    let lifetime = json_u64(v.get("expires_in"))
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30 * 24 * 3600));
    let mtime = fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    let age = SystemTime::now().duration_since(mtime).unwrap_or_default();
    age >= lifetime
}

fn parse_expiry(v: Option<&Value>) -> Option<u64> {
    json_u64(v).or_else(|| {
        v.and_then(|x| x.as_str()).and_then(|s| {
            json_u64(Some(&Value::String(s.trim().into()))).or_else(|| parse_rfc3339_utc(s))
        })
    })
}

fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("+00:00"))
        .unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let time = time.split('.').next().unwrap_or(time);
    let mut t = time.split(':');
    let hh: u32 = t.next()?.parse().ok()?;
    let mm: u32 = t.next()?.parse().ok()?;
    let ss: u32 = t.next()?.parse().ok()?;
    ymd_hms_to_unix(y, m, day, hh, mm, ss)
}

fn ymd_hms_to_unix(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> Option<u64> {
    if !(1..=12).contains(&m) || d == 0 || d > 31 || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era as i64) * 146097 + doe as i64 - 719468;
    let secs = days
        .checked_mul(86400)?
        .checked_add(i64::from(hh) * 3600)?
        .checked_add(i64::from(mm) * 60)?
        .checked_add(i64::from(ss))?;
    u64::try_from(secs).ok()
}

fn jwt_exp(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let mut b64 = payload.replace('-', "+").replace('_', "/");
    while b64.len() % 4 != 0 {
        b64.push('=');
    }
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    json_u64(v.get("exp"))
}

/// Slot Grok CLI uses for SpaceXAI OAuth (`{issuer}::{client_id}`).
pub const SESSION_SLOT: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";

pub struct SessionTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

/// Write `~/.grok/auth.json` in the Grok CLI nested shape. Never logs tokens.
pub fn persist_session_tokens(tokens: &SessionTokens) -> Result<()> {
    persist_session_tokens_at(&auth_json_path(), tokens)
}

pub fn persist_session_tokens_at(path: &Path, tokens: &SessionTokens) -> Result<()> {
    if tokens.access_token.trim().is_empty() {
        return Err(Error::msg("oauth returned an empty access token"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root = match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    let entry = json!({
        "key": tokens.access_token,
        "refresh_token": tokens.refresh_token,
        "expires_at": tokens.expires_at,
        "auth_mode": "oauth",
        "oidc_issuer": "https://auth.x.ai",
    });
    root[SESSION_SLOT] = entry;
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(&root).map_err(|e| Error::msg(e.to_string()))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Same as `grok logout`: drop the cached session file.
pub fn logout_session() -> Result<()> {
    let path = auth_json_path();
    if path.is_file() {
        fs::remove_file(&path).map_err(|_| Error::msg("could not clear grok login session"))?;
    }
    Ok(())
}

fn json_u64(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Config key, else `XAI_API_KEY`, else `HYPER_API_KEY`. Never a placeholder.
pub fn explicit_api_key(cfg: &Config) -> Option<String> {
    explicit_api_key_parts(
        cfg,
        env_key("XAI_API_KEY").or_else(|| env_key("HYPER_API_KEY")),
    )
}

pub(crate) fn explicit_api_key_parts(cfg: &Config, env_key: Option<String>) -> Option<String> {
    if let Some(k) = usable_key(&cfg.server.api_key) {
        return Some(k.to_string());
    }
    env_key.and_then(|k| usable_key(&k).map(str::to_string))
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_local_is_not_a_key() {
        assert!(is_placeholder_key("local"));
        assert!(is_placeholder_key(""));
        assert!(is_placeholder_key("changeme"));
        assert!(usable_key("xai-abc").is_some());
    }

    #[test]
    fn parse_access_token_without_leaking() {
        let dir = tempfile_dir();
        let path = dir.join("auth.json");
        fs::write(&path, r#"{"access_token":"sekrit-token"}"#).unwrap();
        let mat = parse_auth_json(&fs::read_to_string(&path).unwrap(), &path)
            .unwrap()
            .unwrap();
        assert_eq!(mat.token, "sekrit-token");
        assert!(!mat.expired);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_cli_nested_key_slot() {
        let dir = tempfile_dir();
        let path = dir.join("auth.json");
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        fs::write(
            &path,
            format!(
                r#"{{"{SESSION_SLOT}":{{"key":"sess-nested","refresh_token":"ref","expires_at":{exp}}}}}"#
            ),
        )
        .unwrap();
        let mat = parse_auth_json(&fs::read_to_string(&path).unwrap(), &path)
            .unwrap()
            .unwrap();
        assert_eq!(mat.token, "sess-nested");
        assert_eq!(mat.refresh_token, "ref");
        assert!(!mat.expired);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn persist_roundtrip_nested_slot() {
        let dir = tempfile_dir();
        let path = dir.join("auth.json");
        persist_session_tokens_at(
            &path,
            &SessionTokens {
                access_token: "tok-a".into(),
                refresh_token: "ref-a".into(),
                expires_at: 4_000_000_000,
            },
        )
        .unwrap();
        let mat = parse_auth_json(&fs::read_to_string(&path).unwrap(), &path)
            .unwrap()
            .unwrap();
        assert_eq!(mat.token, "tok-a");
        assert_eq!(mat.refresh_token, "ref-a");
        assert!(!mat.expired);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains(SESSION_SLOT));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rfc3339_expiry_parses() {
        assert!(parse_rfc3339_utc("2020-01-01T00:00:00Z").unwrap() < 1_600_000_000);
        assert!(parse_rfc3339_utc("2099-01-01T00:00:00Z").unwrap() > 4_000_000_000);
    }

    #[test]
    fn expired_expires_at_is_flagged() {
        let dir = tempfile_dir();
        let path = dir.join("auth.json");
        fs::write(&path, r#"{"access_token":"old","expires_at":1}"#).unwrap();
        let mat = parse_auth_json(&fs::read_to_string(&path).unwrap(), &path)
            .unwrap()
            .unwrap();
        assert!(mat.expired);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn errors_do_not_include_token() {
        let e = login_err().to_string();
        assert!(e.contains("grok login"));
        assert!(!e.to_ascii_lowercase().contains("token"));
        let e = expired_err().to_string();
        assert!(e.contains("过期") || e.contains("expired"));
        assert!(!e.contains("eyJ"));
    }

    fn tempfile_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("hyper-auth-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }
}
