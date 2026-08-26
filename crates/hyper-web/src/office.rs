//! OnlyOffice Docs bridge: JWT config, tokenized file I/O, forcesave callback.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use hyper_loop::config::OfficeConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, Mutex};

use crate::files::{read_preview, write_workspace_file, FILE_PUT_CAP};
use crate::hub::AppState;

type HmacSha256 = Hmac<Sha256>;

const TOKEN_TTL_SECS: u64 = 12 * 3600;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const FORCESAVE_WAIT: Duration = Duration::from_secs(45);

#[derive(Default)]
pub struct OfficeSaves {
    waiters: Mutex<HashMap<String, Vec<oneshot::Sender<std::result::Result<(), String>>>>>,
}

impl OfficeSaves {
    pub async fn wait(&self, key: &str) -> std::result::Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .await
            .entry(key.to_string())
            .or_default()
            .push(tx);
        match tokio::time::timeout(FORCESAVE_WAIT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("forcesave cancelled".into()),
            Err(_) => Err("文档服务保存超时，请再试一次".into()),
        }
    }

    pub async fn notify(&self, key: &str, result: std::result::Result<(), String>) {
        let waiters = self.waiters.lock().await.remove(key).unwrap_or_default();
        for tx in waiters {
            let _ = tx.send(result.clone());
        }
    }
}

pub fn bridge_router(state: AppState) -> Router {
    Router::new()
        .route("/office/file/{token}", get(bridge_file))
        .route("/office/callback/{token}", post(bridge_callback))
        .with_state(state)
}

pub fn api_routes(r: Router<AppState>) -> Router<AppState> {
    r.route("/office/status", get(office_status))
        .route("/office/config", get(office_config))
        .route("/office/forcesave", post(office_forcesave))
        .route("/office/file/{token}", get(bridge_file))
        .route("/office/callback/{token}", post(bridge_callback))
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

pub async fn docs_ready(docs_url: &str) -> bool {
    let url = format!("{}/healthcheck", docs_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder().timeout(HEALTH_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => {
            let t = r.text().await.unwrap_or_default();
            t.trim().eq_ignore_ascii_case("true") || t.trim() == "1"
        }
        _ => false,
    }
}

async fn office_status(State(st): State<AppState>) -> Json<Value> {
    let (docs_url, has_secret) = {
        let g = st.inner.lock().await;
        (
            g.cfg.office.docs_origin(),
            !g.cfg.office.jwt_secret.trim().is_empty(),
        )
    };
    let ready = has_secret && docs_ready(&docs_url).await;
    let (starting, boot_hint) = st.office_boot.snapshot();
    let hint = if ready {
        Value::Null
    } else if starting {
        json!(if boot_hint.is_empty() {
            "正在启动文档服务…"
        } else {
            boot_hint.as_str()
        })
    } else if !boot_hint.is_empty() {
        json!(boot_hint)
    } else {
        json!("完整编辑暂不可用，当前使用内置预览。")
    };
    Json(json!({
        "ok": true,
        "ready": ready,
        "starting": starting && !ready,
        "docs_url": docs_url,
        "hint": hint,
    }))
}

async fn office_config(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel = q.path.trim();
    if rel.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "path required".into()));
    }
    let (workspace, office, session) = {
        let g = st.inner.lock().await;
        (
            g.session.workspace().to_path_buf(),
            g.cfg.office.clone(),
            g.session.session_id().to_string(),
        )
    };
    if office.jwt_secret.trim().is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "office jwt_secret missing".into(),
        ));
    }
    if !docs_ready(&office.docs_origin()).await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "document server is not ready".into(),
        ));
    }
    let meta = match file_meta(&workspace, rel) {
        Ok(m) => m,
        Err(e) => {
            let io = e.downcast_ref::<std::io::Error>();
            let missing = io.is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound);
            return Err((
                StatusCode::NOT_FOUND,
                if missing {
                    format!("找不到文件：{rel}")
                } else {
                    e.to_string()
                },
            ));
        }
    };
    let key = document_key(rel, meta.mtime_secs, meta.len);
    let ext = file_ext(rel);
    let document_type = document_type(&ext).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unsupported office type: {ext}"),
        )
    })?;
    let file_tok = mint_token(&office.jwt_secret, rel, "f", TOKEN_TTL_SECS)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let cb_tok = mint_token(&office.jwt_secret, rel, "c", TOKEN_TTL_SECS)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let reach = office.reach_origin();
    let cfg = json!({
        "documentType": document_type,
        "type": "desktop",
        "width": "100%",
        "height": "100%",
        "document": {
            "fileType": ext.trim_start_matches('.'),
            "key": key,
            "title": std::path::Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel),
            "url": format!("{reach}/office/file/{file_tok}"),
            "permissions": {
                "edit": true,
                "download": true,
                "print": true,
                "review": true,
                "comment": true,
            }
        },
        "editorConfig": {
            "callbackUrl": format!("{reach}/office/callback/{cb_tok}"),
            "mode": "edit",
            "lang": "zh-CN",
            "region": "zh-CN",
            "user": { "id": session, "name": "hyper" },
            "customization": {
                "autosave": true,
                "forcesave": true,
                "compactHeader": true,
                "feedback": false,
                "help": false,
                "uiTheme": "theme-dark",
            }
        }
    });
    let token = jwt_sign(&office.jwt_secret, &cfg)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut out = cfg;
    out["token"] = json!(token);
    Ok(Json(json!({
        "ok": true,
        "docs_url": office.docs_origin(),
        "key": key,
        "config": out,
    })))
}

#[derive(Deserialize)]
struct ForcesaveBody {
    path: String,
    key: String,
}

async fn office_forcesave(
    State(st): State<AppState>,
    Json(body): Json<ForcesaveBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if body.path.trim().is_empty() || body.key.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "path and key required".into()));
    }
    let office = {
        let g = st.inner.lock().await;
        g.cfg.office.clone()
    };
    command_forcesave(&office, &body.key)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    st.office_saves
        .wait(&body.key)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(
        json!({"ok": true, "path": body.path, "key": body.key}),
    ))
}

async fn bridge_file(
    State(st): State<AppState>,
    AxumPath(token): AxumPath<String>,
) -> Result<Response, (StatusCode, String)> {
    let office = {
        let g = st.inner.lock().await;
        g.cfg.office.clone()
    };
    let claims = verify_token(&office.jwt_secret, &token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;
    if claims.k != "f" {
        return Err((StatusCode::FORBIDDEN, "wrong token kind".into()));
    }
    let workspace = {
        let g = st.inner.lock().await;
        g.session.workspace().to_path_buf()
    };
    let rel = claims.p;
    let (mime, body, _trunc) =
        tokio::task::spawn_blocking(move || read_preview(&workspace, &rel, FILE_PUT_CAP))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, crate::files::file_content_type(&mime));
    Ok((headers, body).into_response())
}

#[derive(Deserialize)]
struct CallbackBody {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

async fn bridge_callback(
    State(st): State<AppState>,
    AxumPath(token): AxumPath<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let office = {
        let g = st.inner.lock().await;
        g.cfg.office.clone()
    };
    let claims = match verify_token(&office.jwt_secret, &token) {
        Ok(c) if c.k == "c" => c,
        _ => return Json(json!({"error": 1})),
    };
    let payload = match decode_callback(&office.jwt_secret, &body) {
        Ok(v) => v,
        Err(_) => return Json(json!({"error": 1})),
    };
    let cb: CallbackBody = match serde_json::from_value(payload) {
        Ok(v) => v,
        Err(_) => return Json(json!({"error": 1})),
    };
    let status = cb.status.unwrap_or(0);
    let key = cb.key.unwrap_or_default();
    // 1 = editing, 4 = closed with no changes
    if status == 1 || status == 4 {
        return Json(json!({"error": 0}));
    }
    if status == 3 || status == 7 {
        if !key.is_empty() {
            st.office_saves
                .notify(&key, Err("文档服务保存失败".into()))
                .await;
        }
        return Json(json!({"error": 0}));
    }
    // 2 = ready to save, 6 = forcesave
    if status != 2 && status != 6 {
        return Json(json!({"error": 0}));
    }
    let Some(url) = cb.url.filter(|s| !s.trim().is_empty()) else {
        return Json(json!({"error": 1}));
    };
    let fetch = rewrite_docs_file_url(&url, &office.docs_origin());
    let bytes = match download_edited(&fetch).await {
        Ok(b) => b,
        Err(e) => {
            if !key.is_empty() {
                st.office_saves.notify(&key, Err(e.to_string())).await;
            }
            return Json(json!({"error": 1}));
        }
    };
    let workspace = {
        let g = st.inner.lock().await;
        g.session.workspace().to_path_buf()
    };
    let rel = claims.p.clone();
    let written = tokio::task::spawn_blocking({
        let rel = rel.clone();
        let bytes = bytes.clone();
        move || write_workspace_file(&workspace, &rel, &bytes)
    })
    .await;
    match written {
        Ok(Ok((path, n, sha))) => {
            let kind = edit_kind(&file_ext(&path));
            let _ = st
                .rpc(
                    "session.user_edit",
                    Some(json!({
                        "path": path,
                        "kind": kind,
                        "bytes": n,
                        "sha256": sha,
                    })),
                )
                .await;
            if !key.is_empty() {
                st.office_saves.notify(&key, Ok(())).await;
            }
            Json(json!({"error": 0}))
        }
        Ok(Err(e)) => {
            if !key.is_empty() {
                st.office_saves.notify(&key, Err(e.to_string())).await;
            }
            Json(json!({"error": 1}))
        }
        Err(e) => {
            if !key.is_empty() {
                st.office_saves.notify(&key, Err(e.to_string())).await;
            }
            Json(json!({"error": 1}))
        }
    }
}

fn decode_callback(secret: &str, body: &Value) -> Result<Value> {
    if let Some(tok) = body.get("token").and_then(|v| v.as_str()) {
        if let Ok(payload) = jwt_verify(secret, tok) {
            return Ok(payload);
        }
    }
    Ok(body.clone())
}

async fn command_forcesave(office: &OfficeConfig, key: &str) -> Result<()> {
    let payload = json!({ "c": "forcesave", "key": key });
    let token = jwt_sign(&office.jwt_secret, &payload)?;
    let mut body = payload;
    body["token"] = json!(token);
    let url = format!("{}/coauthoring/CommandService.ashx", office.docs_origin());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let r = client.post(url).json(&body).send().await?;
    let v: Value = r.json().await.unwrap_or(json!({}));
    let err = v.get("error").and_then(|e| e.as_i64()).unwrap_or(-1);
    if err != 0 {
        bail!("forcesave error {err}");
    }
    Ok(())
}

async fn download_edited(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let r = client.get(url).send().await?;
    if !r.status().is_success() {
        bail!("download edited file: HTTP {}", r.status());
    }
    let bytes = r.bytes().await?;
    if bytes.len() > FILE_PUT_CAP {
        bail!("edited file too large");
    }
    Ok(bytes.to_vec())
}

struct FileMeta {
    mtime_secs: u64,
    len: u64,
}

fn file_meta(root: &Path, rel: &str) -> Result<FileMeta> {
    let path = crate::files::safe_join(root, rel)?;
    let meta = std::fs::metadata(&path)?;
    if meta.is_dir() {
        bail!("is a directory");
    }
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(FileMeta {
        mtime_secs,
        len: meta.len(),
    })
}

pub fn document_key(path: &str, mtime_secs: u64, len: u64) -> String {
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(b"\0");
    h.update(mtime_secs.to_string().as_bytes());
    h.update(b"\0");
    h.update(len.to_string().as_bytes());
    format!("{:x}", h.finalize())
}

pub fn file_ext(path: &str) -> String {
    let n = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    let i = n.rfind('.').unwrap_or(n.len());
    n[i..].to_string()
}

pub fn document_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        ".docx" | ".doc" | ".docm" | ".odt" | ".rtf" | ".txt" => "word",
        ".xlsx" | ".xlsm" | ".xls" | ".csv" | ".ods" => "cell",
        ".pptx" | ".ppt" | ".ppsx" | ".pptm" | ".odp" => "slide",
        ".pdf" => "pdf",
        _ => return None,
    })
}

fn edit_kind(ext: &str) -> &'static str {
    match document_type(ext) {
        Some("cell") => "sheet",
        Some("slide") => "ppt",
        Some(k) => k,
        None => "word",
    }
}

pub fn rewrite_docs_file_url(raw: &str, docs_url: &str) -> String {
    let Ok(u) = reqwest::Url::parse(raw) else {
        return raw.to_string();
    };
    let Ok(mut base) = reqwest::Url::parse(docs_url) else {
        return raw.to_string();
    };
    base.set_path(u.path());
    base.set_query(u.query());
    base.to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TokenClaims {
    p: String,
    e: u64,
    k: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mint_token(secret: &str, path: &str, kind: &str, ttl: u64) -> Result<String> {
    let claims = TokenClaims {
        p: path.to_string(),
        e: now_secs().saturating_add(ttl),
        k: kind.to_string(),
    };
    let payload = serde_json::to_vec(&claims)?;
    let body = URL_SAFE_NO_PAD.encode(&payload);
    let mac = hmac_hex(secret, body.as_bytes())?;
    Ok(format!("{body}.{mac}"))
}

fn verify_token(secret: &str, token: &str) -> Result<TokenClaims> {
    let (body, mac) = token.rsplit_once('.').ok_or_else(|| anyhow!("bad token"))?;
    let expect = hmac_hex(secret, body.as_bytes())?;
    if !ct_eq(mac.as_bytes(), expect.as_bytes()) {
        bail!("bad token mac");
    }
    let raw = URL_SAFE_NO_PAD.decode(body.as_bytes())?;
    let claims: TokenClaims = serde_json::from_slice(&raw)?;
    if claims.e < now_secs() {
        bail!("token expired");
    }
    Ok(claims)
}

fn hmac_hex(secret: &str, data: &[u8]) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| anyhow!("{e}"))?;
    mac.update(data);
    Ok(hex_lower(&mac.finalize().into_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub fn jwt_sign(secret: &str, payload: &Value) -> Result<String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload)?);
    let signing = format!("{header}.{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| anyhow!("{e}"))?;
    mac.update(signing.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{signing}.{sig}"))
}

pub fn jwt_verify(secret: &str, token: &str) -> Result<Value> {
    let mut parts = token.split('.');
    let header = parts.next().ok_or_else(|| anyhow!("bad jwt"))?;
    let body = parts.next().ok_or_else(|| anyhow!("bad jwt"))?;
    let sig = parts.next().ok_or_else(|| anyhow!("bad jwt"))?;
    if parts.next().is_some() {
        bail!("bad jwt");
    }
    let signing = format!("{header}.{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| anyhow!("{e}"))?;
    mac.update(signing.as_bytes());
    let expect = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    if !ct_eq(sig.as_bytes(), expect.as_bytes()) {
        bail!("bad jwt sig");
    }
    let raw = URL_SAFE_NO_PAD.decode(body.as_bytes())?;
    Ok(serde_json::from_slice(&raw)?)
}

/// Persist an empty `[office].jwt_secret` once. Does not rewrite the file when a secret already exists.
pub fn persist_office_secret(cfg_path: &Path) -> Result<OfficeConfig> {
    if cfg_path.exists() {
        if let Ok(disk) = hyper_loop::config::Config::load_from(cfg_path) {
            if !disk.office.jwt_secret.trim().is_empty() {
                return Ok(disk.office);
            }
        }
    }
    let disk = hyper_loop::config::Config::mutate_disk(cfg_path, |c| {
        c.office.fill_secret();
    })?;
    Ok(disk.office)
}

pub async fn spawn_bridge(state: AppState, bind: &str) {
    let addr = match bind.parse::<std::net::SocketAddr>() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("office bridge: bad bind {bind}: {e}");
            return;
        }
    };
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            let bound = listener.local_addr().ok();
            if let Some(a) = bound {
                eprintln!("office bridge  http://{a}/  (OnlyOffice file I/O)");
            }
            if let Err(e) = axum::serve(listener, bridge_router(state)).await {
                eprintln!("office bridge: {e}");
            }
        }
        Err(e) => eprintln!("office bridge skipped ({bind}): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_roundtrip() {
        let secret = "s".repeat(32);
        let payload = json!({"documentType": "cell", "n": 1});
        let tok = jwt_sign(&secret, &payload).unwrap();
        let back = jwt_verify(&secret, &tok).unwrap();
        assert_eq!(back["n"], 1);
        assert!(jwt_verify("other", &tok).is_err());
    }

    #[test]
    fn token_roundtrip_and_kind() {
        let secret = "tok-secret-tok-secret-tok-secret-xx";
        let t = mint_token(secret, "reports/a.xlsx", "f", 60).unwrap();
        let c = verify_token(secret, &t).unwrap();
        assert_eq!(c.p, "reports/a.xlsx");
        assert_eq!(c.k, "f");
        assert!(verify_token("nope", &t).is_err());
    }

    #[test]
    fn document_types() {
        assert_eq!(document_type(".docx"), Some("word"));
        assert_eq!(document_type(".xlsx"), Some("cell"));
        assert_eq!(document_type(".pptx"), Some("slide"));
        assert_eq!(document_type(".pdf"), Some("pdf"));
        assert_eq!(document_type(".xls"), Some("cell"));
        assert_eq!(document_type(".md"), None);
    }

    #[test]
    fn rewrite_localhost_to_published_docs() {
        let out = rewrite_docs_file_url(
            "http://localhost/cache/files/data/x.docx?md5=1",
            "http://127.0.0.1:8080",
        );
        assert_eq!(out, "http://127.0.0.1:8080/cache/files/data/x.docx?md5=1");
    }

    #[test]
    fn document_key_changes_with_mtime() {
        let a = document_key("a.docx", 1, 10);
        let b = document_key("a.docx", 2, 10);
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn persist_secret_writes_once() {
        let dir = std::env::temp_dir().join(format!("hyper-oo-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        hyper_loop::config::Config::default()
            .save_to(&path)
            .unwrap();
        let a = persist_office_secret(&path).unwrap();
        assert_eq!(a.jwt_secret.len(), 64);
        let b = persist_office_secret(&path).unwrap();
        assert_eq!(a.jwt_secret, b.jwt_secret);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[office]"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
