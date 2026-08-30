//! DM pairing overlay. Hermes-shaped: unknown senders get a short code;
//! sending it binds `sender_id` without rewriting `config.toml`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config::Config;

use super::ChannelEndpoint;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Overlay {
    pair_code: String,
    #[serde(default)]
    allow_from: Vec<String>,
}

fn store() -> &'static Mutex<HashMap<String, Overlay>> {
    static S: OnceLock<Mutex<HashMap<String, Overlay>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(ep: &ChannelEndpoint) -> String {
    let id = ep.id.trim();
    if id.is_empty() {
        ep.kind.clone()
    } else {
        id.to_string()
    }
}

fn overlay_path(id: &str) -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        return None;
    }
    Config::home_dir()
        .ok()
        .map(|h| h.join("channels").join(format!("{safe}.pair.json")))
}

fn load(ep: &ChannelEndpoint) -> Overlay {
    let k = key(ep);
    if let Ok(g) = store().lock() {
        if let Some(o) = g.get(&k) {
            return o.clone();
        }
    }
    if let Some(path) = overlay_path(&k) {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(o) = serde_json::from_str::<Overlay>(&raw) {
                if let Ok(mut g) = store().lock() {
                    g.insert(k, o.clone());
                }
                return o;
            }
        }
    }
    Overlay::default()
}

fn save(ep: &ChannelEndpoint, o: Overlay) {
    let k = key(ep);
    if let Ok(mut g) = store().lock() {
        g.insert(k.clone(), o.clone());
    }
    if let Some(path) = overlay_path(&k) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&o) {
            let _ = fs::write(path, bytes);
        }
    }
}

pub fn is_allowed(ep: &ChannelEndpoint, sender: &str) -> bool {
    if sender.is_empty() {
        return false;
    }
    if ep.allow_from.iter().any(|a| a == sender) {
        return true;
    }
    load(ep).allow_from.iter().any(|a| a == sender)
}

pub fn ensure_code(ep: &ChannelEndpoint) -> String {
    let mut o = load(ep);
    if o.pair_code.is_empty() {
        o.pair_code = generate();
        save(ep, o.clone());
    }
    o.pair_code
}

/// Bind `sender` when `text` is the pairing code. Returns true on success.
pub fn try_pair(ep: &ChannelEndpoint, sender: &str, text: &str) -> bool {
    if sender.is_empty() {
        return false;
    }
    let code = ensure_code(ep);
    let t = text.trim();
    if t.is_empty() || !t.eq_ignore_ascii_case(&code) {
        return false;
    }
    let mut o = load(ep);
    if !o.allow_from.iter().any(|a| a == sender) {
        o.allow_from.push(sender.to_string());
        save(ep, o);
    }
    true
}

pub fn hint(ep: &ChannelEndpoint, zh: bool) -> String {
    let code = ensure_code(ep);
    if zh {
        format!("未授权。把配对码发给我以绑定此私信：`{code}`")
    } else {
        format!("Not authorized. Send this pairing code in DM to bind: `{code}`")
    }
}

fn generate() -> String {
    let n = uuid::Uuid::new_v4().as_u128();
    const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut s = String::with_capacity(6);
    let mut x = n;
    for _ in 0..6 {
        s.push(A[(x as usize) % A.len()] as char);
        x /= A.len() as u128;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_code_binds_sender() {
        let ep = ChannelEndpoint {
            id: format!("pair-{}", uuid::Uuid::new_v4().simple()),
            kind: "feishu".into(),
            dm_policy: "allowlist".into(),
            ..ChannelEndpoint::default()
        };
        assert!(!is_allowed(&ep, "ou_1"));
        let code = ensure_code(&ep);
        assert!(!try_pair(&ep, "ou_1", "nope"));
        assert!(try_pair(&ep, "ou_1", &code));
        assert!(is_allowed(&ep, "ou_1"));
        assert!(!is_allowed(&ep, "ou_2"));
    }
}
