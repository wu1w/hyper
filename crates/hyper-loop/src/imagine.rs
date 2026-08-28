//! Imagine REST: `POST {image_base_url}/images/generations`.
//!
//! Used when the session is in imagine mode (console + menu / `/imagine`).
//! Auth follows the chat rail; the URL can be a separate origin.

use std::path::Path;

use serde_json::{json, Value};

use crate::config::Config;
use crate::session::StoredMedia;
use crate::tool_calls::CancelFlag;
use crate::transport::{
    apply_grok_headers, http_error_snippet, is_session_host, resolve_live, GrokTransport,
};

const FETCH_CAP: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageHit {
    pub b64: Option<String>,
    pub url: Option<String>,
    pub revised: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ImagineOut {
    pub stored: Vec<StoredMedia>,
    pub caption: String,
}

pub fn generations_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/images/generations") {
        b.to_string()
    } else {
        format!("{b}/images/generations")
    }
}

pub fn parse_generation_body(v: &Value) -> Result<Vec<ImageHit>, String> {
    if let Some(err) = api_error(v) {
        return Err(err);
    }
    let mut hits = Vec::new();
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        for item in arr {
            if let Some(hit) = hit_from_value(item) {
                hits.push(hit);
            }
        }
    } else if let Some(arr) = v.get("images").and_then(|d| d.as_array()) {
        for item in arr {
            if let Some(hit) = hit_from_value(item) {
                hits.push(hit);
            }
        }
    } else if let Some(hit) = hit_from_value(v) {
        hits.push(hit);
    }
    if hits.is_empty() {
        return Err("no images in response".into());
    }
    Ok(hits)
}

pub async fn generate(
    cfg: &Config,
    prompt: &str,
    workspace: &Path,
    cancel: &CancelFlag,
) -> Result<ImagineOut, String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("image generation needs a text prompt".into());
    }
    if cancel.is_cancelled() {
        return Err("aborted".into());
    }
    let resolved = resolve_live(cfg).await.map_err(|e| e.to_string())?;
    let image_base = cfg.server.resolved_image_base_url();
    if image_base.is_empty() {
        return Err("image endpoint is empty; set 生图端点 or the chat base_url".into());
    }
    let url = generations_url(&image_base);
    let model =
        crate::family::Family::wire_model_id(&cfg.server.resolved_image_model()).to_string();
    let timeout_s = cfg.server.read_timeout_s.clamp(60, 180);
    let client = crate::llm_http::build_client_for(
        crate::llm_http::effective_connect_timeout_s_for(cfg.server.connect_timeout_s, &image_base),
        timeout_s,
        &image_base,
    )
    .map_err(|e| e.to_string())?;
    let body = json!({
        "model": model,
        "prompt": prompt,
        "n": 1,
    });
    let header_mode = if is_session_host(&image_base) {
        resolved.mode
    } else {
        GrokTransport::OpenAiCompat
    };
    let mut req = client.post(&url).json(&body);
    if !resolved.token().is_empty() {
        req = req.bearer_auth(resolved.token());
    }
    req = apply_grok_headers(req, header_mode);
    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("aborted".into()),
        r = req.send() => r.map_err(|e| e.to_string())?,
    };
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        return Err(http_error_snippet(status, &text));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        let snippet = http_error_snippet(status, &text);
        if snippet.is_empty() {
            "image endpoint returned non-JSON".into()
        } else {
            format!("image endpoint: {snippet}")
        }
    })?;
    let hits = parse_generation_body(&value)?;
    let caption = hits
        .iter()
        .find_map(|h| h.revised.as_deref())
        .unwrap_or("")
        .trim()
        .to_string();
    let stored = materialize_hits(workspace, &hits, FETCH_CAP).await;
    if stored.is_empty() {
        return Err("could not decode or save generated image".into());
    }
    Ok(ImagineOut { stored, caption })
}

pub fn persist_b64(root: &Path, raw: &str) -> Option<StoredMedia> {
    let (mime, bytes) = crate::media::decode_image_payload(raw)?;
    let url = crate::media::persist_image_file(root, &bytes, &mime)?;
    Some(StoredMedia {
        kind: "image".into(),
        mime,
        url,
    })
}

async fn materialize_hits(root: &Path, hits: &[ImageHit], cap: usize) -> Vec<StoredMedia> {
    let mut out = Vec::new();
    for hit in hits {
        if let Some(b64) = hit.b64.as_deref() {
            if let Some(m) = persist_b64(root, b64) {
                out.push(m);
                continue;
            }
        }
        if let Some(url) = hit.url.as_deref() {
            let u = url.trim();
            if u.starts_with("data:") {
                if let Some(m) = persist_b64(root, u) {
                    out.push(m);
                    continue;
                }
            }
            if u.starts_with("http://") || u.starts_with("https://") {
                if let Ok((mime, bytes)) = crate::media::fetch_http_bytes(u, cap).await {
                    if let Some(rel) = crate::media::persist_image_file(root, &bytes, &mime) {
                        out.push(StoredMedia {
                            kind: "image".into(),
                            mime,
                            url: rel,
                        });
                        continue;
                    }
                }
                out.push(StoredMedia {
                    kind: "image".into(),
                    mime: "image/jpeg".into(),
                    url: u.to_string(),
                });
            }
        }
    }
    out
}

fn api_error(v: &Value) -> Option<String> {
    let err = v.get("error")?;
    if let Some(s) = err.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    err.get("message")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn hit_from_value(v: &Value) -> Option<ImageHit> {
    if v.is_null() {
        return None;
    }
    if let Some(s) = v.as_str() {
        return hit_from_strings(Some(s), None, None);
    }
    let b64 = first_str(v, &["b64_json", "b64", "base64", "image_base64", "result"]);
    let url = first_str(v, &["url", "image_url", "image"]).or_else(|| {
        v.get("image_url")
            .and_then(|u| u.get("url"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
    });
    let revised = first_str(v, &["revised_prompt", "revisedPrompt"]);
    hit_from_strings(b64.as_deref(), url.as_deref(), revised.as_deref())
}

fn hit_from_strings(
    b64: Option<&str>,
    url: Option<&str>,
    revised: Option<&str>,
) -> Option<ImageHit> {
    let b64 = b64
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let url = url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let revised = revised
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if b64.is_none() && url.is_none() {
        return None;
    }
    Some(ImageHit { b64, url, revised })
}

fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        match v.get(*k) {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.clone()),
            Some(Value::Object(map)) => {
                if let Some(Value::String(s)) = map.get("b64_json").or_else(|| map.get("url")) {
                    if !s.trim().is_empty() {
                        return Some(s.clone());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::PROBE_IMAGE_B64;
    use serde_json::json;

    #[test]
    fn generations_url_joins_and_is_idempotent() {
        assert_eq!(
            generations_url("https://api.x.ai/v1/"),
            "https://api.x.ai/v1/images/generations"
        );
        assert_eq!(
            generations_url("https://api.x.ai/v1/images/generations"),
            "https://api.x.ai/v1/images/generations"
        );
    }

    #[test]
    fn parse_openai_b64_and_url() {
        let b64 = parse_generation_body(&json!({
            "data": [{ "b64_json": PROBE_IMAGE_B64, "revised_prompt": "a red square" }]
        }))
        .unwrap();
        assert_eq!(b64[0].b64.as_deref(), Some(PROBE_IMAGE_B64));
        assert_eq!(b64[0].revised.as_deref(), Some("a red square"));

        let url = parse_generation_body(&json!({
            "data": [{ "url": "https://cdn.example/a.png" }]
        }))
        .unwrap();
        assert_eq!(url[0].url.as_deref(), Some("https://cdn.example/a.png"));
    }

    #[test]
    fn parse_error_body() {
        let err = parse_generation_body(&json!({
            "error": { "message": "model not found" }
        }))
        .unwrap_err();
        assert!(err.contains("model not found"), "{err}");
        assert!(parse_generation_body(&json!({ "data": [] })).is_err());
    }

    #[test]
    fn persist_probe_png() {
        let dir =
            std::env::temp_dir().join(format!("hyper-imagine-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let stored = persist_b64(&dir, PROBE_IMAGE_B64).unwrap();
        assert_eq!(stored.kind, "image");
        assert_eq!(stored.mime, "image/png");
        assert!(stored.url.contains(".grok-hyper/generated/imagine-"));
        let path = dir.join(&stored.url);
        assert!(path.is_file(), "{}", path.display());
        let _ = std::fs::remove_dir_all(dir);
    }
}
