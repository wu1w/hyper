//! WeChat iLink CDN: AES-128-ECB + PKCS7, getuploadurl, download.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use md5::{Digest, Md5};
use serde_json::{json, Value};

use crate::error::{Error, Result};

use super::xfer::{guess_mime, Kind};

pub const CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

#[derive(Clone, Debug)]
pub struct CdnMedia {
    pub encrypt_query_param: String,
    pub aes_key_b64: String,
    pub cipher_len: usize,
    pub raw_len: usize,
    pub kind: Kind,
    pub name: String,
    #[allow(dead_code)]
    pub mime: String,
}

#[derive(Clone, Debug)]
pub struct CdnRef {
    pub param: String,
    pub aes_key: String,
    pub aeskey_hex: String,
    pub kind: Kind,
    pub name: String,
}

pub fn media_type_num(kind: Kind) -> i32 {
    match kind {
        Kind::Image => 1,
        Kind::Video => 2,
        Kind::File => 3,
        Kind::Audio => 4,
    }
}

pub fn encrypt_aes_ecb(plain: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let padded = pkcs7_pad(plain);
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks_exact(16) {
        let mut block = *aes::Block::from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

pub fn decrypt_aes_ecb(cipher_bytes: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    if cipher_bytes.is_empty() || cipher_bytes.len() % 16 != 0 {
        return Err(Error::msg("wechat cdn: ciphertext not block-aligned"));
    }
    let cipher = Aes128::new(key.into());
    let mut out = Vec::with_capacity(cipher_bytes.len());
    for chunk in cipher_bytes.chunks_exact(16) {
        let mut block = *aes::Block::from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    pkcs7_unpad(&out)
}

fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad = 16 - (data.len() % 16);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad as u8).take(pad));
    out
}

fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>> {
    let pad = *data.last().ok_or_else(|| Error::msg("wechat cdn: empty"))? as usize;
    if pad == 0 || pad > 16 || data.len() < pad {
        return Err(Error::msg("wechat cdn: bad pkcs7"));
    }
    if !data[data.len() - pad..].iter().all(|b| *b as usize == pad) {
        return Err(Error::msg("wechat cdn: bad pkcs7"));
    }
    Ok(data[..data.len() - pad].to_vec())
}

fn cipher_len(raw: usize) -> usize {
    ((raw + 1 + 15) / 16) * 16
}

pub fn md5_hex(bytes: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

pub fn random_key() -> [u8; 16] {
    let u = uuid::Uuid::new_v4();
    let b = u.as_bytes();
    let mut k = [0u8; 16];
    k.copy_from_slice(b);
    k
}

pub fn key_hex(key: &[u8; 16]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// Official openclaw: `aes_key` in the message is base64(hex ASCII).
pub fn key_to_wire_b64(key: &[u8; 16]) -> String {
    STANDARD.encode(key_hex(key).as_bytes())
}

pub fn parse_aes_key(aes_key: &str, aeskey_hex: &str) -> Result<[u8; 16]> {
    let hex = aeskey_hex.trim();
    if hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return hex_key(hex);
    }
    let raw = aes_key.trim();
    if raw.len() == 32 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return hex_key(raw);
    }
    let decoded = STANDARD
        .decode(raw.as_bytes())
        .map_err(|_| Error::msg("wechat cdn: aes_key b64"))?;
    if decoded.len() == 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&decoded);
        return Ok(k);
    }
    if decoded.len() == 32 && decoded.iter().all(|b| b.is_ascii_hexdigit()) {
        let s = String::from_utf8(decoded).map_err(|_| Error::msg("wechat cdn: aes_key utf8"))?;
        return hex_key(&s);
    }
    Err(Error::msg("wechat cdn: unrecognized aes key"))
}

fn hex_key(s: &str) -> Result<[u8; 16]> {
    let mut k = [0u8; 16];
    for i in 0..16 {
        k[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::msg("wechat cdn: aes hex"))?;
    }
    Ok(k)
}

pub fn upload_url(param: &str, filekey: &str) -> String {
    format!(
        "{CDN_BASE}/upload?encrypted_query_param={}&filekey={filekey}",
        urlencoding_query(param)
    )
}

pub fn download_url(param: &str) -> String {
    format!(
        "{CDN_BASE}/download?encrypted_query_param={}",
        urlencoding_query(param)
    )
}

fn urlencoding_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn getupload_body(
    filekey: &str,
    kind: Kind,
    to: &str,
    raw: &[u8],
    cipher: &[u8],
    aeskey_hex: &str,
) -> Value {
    json!({
        "filekey": filekey,
        "media_type": media_type_num(kind),
        "to_user_id": to,
        "rawsize": raw.len(),
        "rawfilemd5": md5_hex(raw),
        "filesize": cipher.len(),
        "no_need_thumb": true,
        "aeskey": aeskey_hex,
        "base_info": {"channel_version": "2.2.0"},
    })
}

pub async fn download(http: &reqwest::Client, refer: &CdnRef) -> Result<Vec<u8>> {
    if refer.param.trim().is_empty() {
        return Err(Error::msg("wechat cdn: missing encrypt_query_param"));
    }
    let key = parse_aes_key(&refer.aes_key, &refer.aeskey_hex)?;
    let url = download_url(&refer.param);
    let resp = http.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!("wechat cdn download {}", resp.status())));
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > super::xfer::FETCH_CAP {
        return Err(Error::msg("wechat cdn: file over cap"));
    }
    decrypt_aes_ecb(&bytes, &key)
}

/// `get_url` posts getuploadurl JSON and returns the API object.
pub async fn upload_with<F, Fut>(
    http: &reqwest::Client,
    get_url: F,
    to: &str,
    kind: Kind,
    name: &str,
    bytes: &[u8],
) -> Result<CdnMedia>
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    if bytes.len() > super::xfer::FETCH_CAP {
        return Err(Error::msg("wechat cdn: file over cap"));
    }
    let key = random_key();
    let hex = key_hex(&key);
    let cipher = encrypt_aes_ecb(bytes, &key);
    let filekey = uuid::Uuid::new_v4().simple().to_string();
    let data = get_url(getupload_body(&filekey, kind, to, bytes, &cipher, &hex)).await?;
    let param = js_str(&data["upload_param"]);
    let full = js_str(&data["upload_full_url"]);
    if param.is_empty() && full.is_empty() {
        return Err(Error::msg(format!("wechat getuploadurl: {data}")));
    }
    let url = if !full.is_empty() {
        full
    } else {
        upload_url(&param, &filekey)
    };
    let resp = http
        .post(&url)
        .header("Content-Type", "application/octet-stream")
        .body(cipher.clone())
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!("wechat cdn upload {}", resp.status())));
    }
    let enc = resp
        .headers()
        .get("x-encrypted-param")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if enc.is_empty() {
        return Err(Error::msg("wechat cdn upload: missing x-encrypted-param"));
    }
    Ok(CdnMedia {
        encrypt_query_param: enc,
        aes_key_b64: key_to_wire_b64(&key),
        cipher_len: cipher.len(),
        raw_len: bytes.len(),
        kind,
        name: name.to_string(),
        mime: guess_mime(name, kind).into(),
    })
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

pub fn item_from_cdn(media: &CdnMedia) -> Value {
    let cdn = json!({
        "encrypt_query_param": media.encrypt_query_param,
        "aes_key": media.aes_key_b64,
        "encrypt_type": 1,
    });
    match media.kind {
        Kind::Image => json!({
            "type": 2,
            "image_item": {
                "media": cdn,
                "mid_size": media.cipher_len,
            }
        }),
        Kind::Video => json!({
            "type": 5,
            "video_item": {
                "media": cdn,
                "video_size": media.cipher_len,
            }
        }),
        Kind::Audio => json!({
            "type": 3,
            "voice_item": { "media": cdn }
        }),
        Kind::File => json!({
            "type": 4,
            "file_item": {
                "media": cdn,
                "file_name": media.name,
                "len": media.raw_len,
            }
        }),
    }
}

pub fn cdn_ref_from_item(item: &Value) -> Option<CdnRef> {
    let ty = match item.get("type") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };
    let (kind, obj, name_keys) = match ty {
        2 => (Kind::Image, &item["image_item"], &["file_name"][..]),
        3 => (Kind::Audio, &item["voice_item"], &["file_name"][..]),
        4 => (Kind::File, &item["file_item"], &["file_name", "media"][..]),
        5 => (Kind::Video, &item["video_item"], &["file_name"][..]),
        _ => return None,
    };
    let media = &obj["media"];
    let param = js_str(&media["encrypt_query_param"]);
    if param.is_empty() {
        return None;
    }
    let mut name = js_str(&obj["file_name"]);
    if name.is_empty() {
        name = js_str(&media["file_name"]);
    }
    if name.is_empty() {
        let _ = name_keys;
        name = match kind {
            Kind::Image => "image.jpg".into(),
            Kind::Audio => "voice.silk".into(),
            Kind::Video => "video.mp4".into(),
            Kind::File => "file.bin".into(),
        };
    }
    Some(CdnRef {
        param,
        aes_key: js_str(&media["aes_key"]),
        aeskey_hex: js_str(&obj["aeskey"]),
        kind,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ecb() {
        let key = *b"0011223344556677";
        let msg = b"hello wechat cdn";
        let ct = encrypt_aes_ecb(msg, &key);
        assert_eq!(ct.len(), cipher_len(msg.len()));
        assert_eq!(decrypt_aes_ecb(&ct, &key).unwrap(), msg);
    }

    #[test]
    fn parse_hex_and_b64_hex() {
        let hex = "00112233445566778899aabbccddeeff";
        let k = parse_aes_key("", hex).unwrap();
        assert_eq!(key_hex(&k), hex);
        let wire = STANDARD.encode(hex.as_bytes());
        let k2 = parse_aes_key(&wire, "").unwrap();
        assert_eq!(k2, k);
        let raw_b64 = STANDARD.encode(&k);
        let k3 = parse_aes_key(&raw_b64, "").unwrap();
        assert_eq!(k3, k);
    }

    #[test]
    fn item_image_shape() {
        let m = CdnMedia {
            encrypt_query_param: "AAF".into(),
            aes_key_b64: "QQ==".into(),
            cipher_len: 16,
            raw_len: 1,
            kind: Kind::Image,
            name: "a.jpg".into(),
            mime: "image/jpeg".into(),
        };
        let v = item_from_cdn(&m);
        assert_eq!(v["type"], 2);
        assert_eq!(v["image_item"]["mid_size"], 16);
    }
}
