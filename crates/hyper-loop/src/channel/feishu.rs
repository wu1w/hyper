//! Feishu / Lark bot adapter. Receive via long-connection WS, send via Open API HTTP.
//!
//! No `lark-oapi` crate: tenant token + HTTP send + WS. Official long-connection
//! frames are pbbp2 protobuf wrapping a JSON `im.message.receive_v1` payload.
//! Text JSON frames are also accepted. Never log `app_secret` or WS tickets.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const FEISHU_BASE: &str = "https://open.feishu.cn";
const LARK_BASE: &str = "https://open.larksuite.com";
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);
const RECONNECT_WAIT: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Replay only recent downtime, not the whole chat history on first boot.
const CATCHUP_MAX_AGE_MS: u64 = 30 * 60 * 1000;
/// Drop WS/catch-up duplicates of the same inbound `message_id` in one process.
const INBOUND_SEEN_CAP: usize = 2048;

fn feishu_trace(msg: impl std::fmt::Display) {
    let line = format!("{msg}");
    eprintln!("{line}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let Ok(home) = crate::config::Config::home_dir() else {
        return;
    };
    let path = home.join("logs").join("feishu.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{ts} {line}");
    }
}

const PB_CONTROL: i32 = 0;
const PB_DATA: i32 = 1;

type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

#[derive(Clone)]
struct FeishuApi {
    http: reqwest::Client,
    app_id: String,
    secret: String,
    base: String,
}

static TOKEN: StdMutex<Option<CachedToken>> = StdMutex::new(None);

struct CachedToken {
    app_id: String,
    base: String,
    token: String,
    until: Instant,
}

#[derive(Clone, Default)]
struct PbHeader {
    key: String,
    value: String,
}

#[derive(Clone, Default)]
struct PbFrame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    method: i32,
    headers: Vec<PbHeader>,
    payload_encoding: String,
    payload_type: String,
    payload: Vec<u8>,
    log_id_new: String,
}

impl PbFrame {
    fn header(&self, key: &str) -> &str {
        self.headers
            .iter()
            .find(|h| h.key.eq_ignore_ascii_case(key))
            .map(|h| h.value.as_str())
            .unwrap_or("")
    }

    fn ping(service: i32) -> Self {
        Self {
            service,
            method: PB_CONTROL,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            ..Self::default()
        }
    }

    fn to_pong(&self) -> Self {
        let mut f = self.clone();
        f.method = PB_CONTROL;
        if let Some(h) = f
            .headers
            .iter_mut()
            .find(|h| h.key.eq_ignore_ascii_case("type"))
        {
            h.value = "pong".into();
        } else {
            f.headers.push(PbHeader {
                key: "type".into(),
                value: "pong".into(),
            });
        }
        f
    }

    fn to_ack(&self) -> Self {
        let mut f = self.clone();
        f.payload = br#"{"code":200}"#.to_vec();
        if !f
            .headers
            .iter()
            .any(|h| h.key.eq_ignore_ascii_case("biz_rt"))
        {
            f.headers.push(PbHeader {
                key: "biz_rt".into(),
                value: "0".into(),
            });
        }
        f
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        pb_u64(1, self.seq_id, &mut out);
        pb_u64(2, self.log_id, &mut out);
        pb_i32(3, self.service, &mut out);
        pb_i32(4, self.method, &mut out);
        for h in &self.headers {
            let mut inner = Vec::new();
            pb_bytes(1, h.key.as_bytes(), &mut inner);
            pb_bytes(2, h.value.as_bytes(), &mut inner);
            pb_bytes(5, &inner, &mut out);
        }
        if !self.payload_encoding.is_empty() {
            pb_bytes(6, self.payload_encoding.as_bytes(), &mut out);
        }
        if !self.payload_type.is_empty() {
            pb_bytes(7, self.payload_type.as_bytes(), &mut out);
        }
        if !self.payload.is_empty() {
            pb_bytes(8, &self.payload, &mut out);
        }
        if !self.log_id_new.is_empty() {
            pb_bytes(9, self.log_id_new.as_bytes(), &mut out);
        }
        out
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        let mut f = Self::default();
        let mut i = 0usize;
        while i < buf.len() {
            let key = pb_varint(buf, &mut i)?;
            let field = (key >> 3) as u32;
            let wire = (key & 7) as u32;
            match (field, wire) {
                (1, 0) => f.seq_id = pb_varint(buf, &mut i)?,
                (2, 0) => f.log_id = pb_varint(buf, &mut i)?,
                (3, 0) => f.service = pb_varint(buf, &mut i)? as i32,
                (4, 0) => f.method = pb_varint(buf, &mut i)? as i32,
                (5, 2) => {
                    let b = pb_len(buf, &mut i)?;
                    f.headers.push(decode_header(b)?);
                }
                (6, 2) => f.payload_encoding = pb_string(buf, &mut i)?,
                (7, 2) => f.payload_type = pb_string(buf, &mut i)?,
                (8, 2) => f.payload = pb_len(buf, &mut i)?.to_vec(),
                (9, 2) => f.log_id_new = pb_string(buf, &mut i)?,
                (_, w) => pb_skip(buf, &mut i, w)?,
            }
        }
        Some(f)
    }
}

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let app_id = extra(ep, "app_id");
    let app_id = if app_id.is_empty() {
        extra(ep, "client_id")
    } else {
        app_id
    };
    let secret = extra(ep, "app_secret");
    let secret = if secret.is_empty() {
        extra(ep, "client_secret")
    } else {
        secret
    };
    if app_id.is_empty() || secret.is_empty() {
        None
    } else {
        Some((app_id, secret))
    }
}

fn open_base(ep: &ChannelEndpoint) -> &'static str {
    let domain = extra(ep, "domain").to_ascii_lowercase();
    let brand = extra(ep, "tenant_brand").to_ascii_lowercase();
    if domain.contains("lark") || brand == "lark" {
        LARK_BASE
    } else {
        FEISHU_BASE
    }
}

pub async fn run_ws(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((app_id, secret)) = credentials(&ep) else {
        return Err(Error::msg(
            "feishu: extra.app_id and extra.app_secret required",
        ));
    };
    let base = open_base(&ep);
    let http = crate::llm_http::env_aware_client(20, base)?;
    eprintln!("hyper feishu gateway starting app_id={app_id} base={base}");
    loop {
        match run_once(&http, &ep, &mgr, &app_id, &secret, base).await {
            Ok(()) => eprintln!("hyper feishu: socket closed, reconnecting"),
            Err(e) => eprintln!("hyper feishu: {e}; retry in 2s"),
        }
        tokio::time::sleep(RECONNECT_WAIT).await;
    }
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return Err(Error::msg("feishu send: missing credentials"));
    };
    let base = open_base(ep);
    let http = crate::llm_http::env_aware_client(30, base)?;
    let text = super::xfer::spoken_text(parts);
    if !text.trim().is_empty() {
        promote_or_send_text(&http, env, base, &app_id, &secret, &text).await?;
    }
    for part in parts {
        if matches!(part, ContentPart::Text { .. }) {
            continue;
        }
        if let Err(e) = send_media(&http, env, base, &app_id, &secret, part).await {
            eprintln!("hyper feishu media: {e}");
            let line = part.fallback_line().unwrap_or_else(|| "[文件]".into());
            let _ = send_text(&http, env, base, &app_id, &secret, &line).await;
        }
    }
    Ok(())
}

pub(crate) async fn send_choices(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    text: &str,
    buttons: &[(String, String)],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return Err(Error::msg("feishu send: missing credentials"));
    };
    let base = open_base(ep);
    let http = crate::llm_http::env_aware_client(30, base)?;
    let card = feishu_choice_card(text, buttons);
    send_im(&http, env, base, &app_id, &secret, "interactive", card)
        .await
        .map(|_| ())
}

fn feishu_choice_card(text: &str, buttons: &[(String, String)]) -> Value {
    let actions: Vec<Value> = buttons
        .iter()
        .enumerate()
        .map(|(i, (id, label))| {
            let style = match (i, buttons.len()) {
                (0, _) => "primary",
                (i, n) if i + 1 == n && n >= 3 => "danger",
                _ => "default",
            };
            json!({
                "tag": "button",
                "text": { "tag": "plain_text", "content": label },
                "type": style,
                "value": { "choice": id },
            })
        })
        .collect();
    json!({
        "config": { "wide_screen_mode": true },
        "elements": [
            { "tag": "div", "text": { "tag": "lark_md", "content": text } },
            { "tag": "action", "actions": actions },
        ]
    })
}

/// ACK / think / tool lines: one message, updated in place (Hermes-style).
pub(crate) async fn send_progress(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return Ok(());
    };
    let text = super::xfer::spoken_text(parts);
    if text.trim().is_empty() {
        return Ok(());
    }
    let base = open_base(ep);
    let http = crate::llm_http::env_aware_client(15, base)?;
    upsert_progress_text(&http, env, base, &app_id, &secret, &text).await
}

const TYPING_EMOJI: &str = "Typing";

fn typing_message_id(env: &NativePayload) -> String {
    js_str(env.meta.get("message_id").unwrap_or(&Value::Null))
}

fn typing_ok(env: &NativePayload) -> bool {
    !typing_message_id(env).is_empty()
}

fn typing_create_body() -> Value {
    json!({ "reaction_type": { "emoji_type": TYPING_EMOJI } })
}

fn typing_create_url(base: &str, message_id: &str) -> String {
    format!(
        "{base}/open-apis/im/v1/messages/{}/reactions",
        urlencoding_seg(message_id)
    )
}

fn typing_delete_url(base: &str, message_id: &str, reaction_id: &str) -> String {
    format!(
        "{base}/open-apis/im/v1/messages/{}/reactions/{}",
        urlencoding_seg(message_id),
        urlencoding_seg(reaction_id)
    )
}

fn parse_reaction_id(data: &Value) -> Option<String> {
    let id = js_str(&data["data"]["reaction_id"]);
    if !id.is_empty() {
        return Some(id);
    }
    let id = js_str(&data["reaction_id"]);
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn feishu_api_ok(data: &Value) -> bool {
    match data.get("code") {
        None => true,
        Some(Value::Number(n)) => n.as_i64() == Some(0) || n.as_u64() == Some(0),
        Some(Value::String(s)) => s.trim() == "0",
        _ => false,
    }
}

fn typing_reactions() -> &'static StdMutex<HashMap<String, String>> {
    static C: OnceLock<StdMutex<HashMap<String, String>>> = OnceLock::new();
    C.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn cached_reaction(message_id: &str) -> Option<String> {
    let Ok(g) = typing_reactions().lock() else {
        return None;
    };
    g.get(message_id).cloned()
}

fn store_reaction(message_id: &str, reaction_id: String) {
    let Ok(mut g) = typing_reactions().lock() else {
        return;
    };
    g.insert(message_id.to_string(), reaction_id);
}

fn take_reaction(message_id: &str) -> Option<String> {
    let Ok(mut g) = typing_reactions().lock() else {
        return None;
    };
    g.remove(message_id)
}

/// Hermes: `Typing` reaction on the inbound message while the agent runs.
/// Failures are silent. Call once per turn; [`stop_typing`] removes it.
pub(crate) async fn send_typing(ep: Option<&ChannelEndpoint>, env: &NativePayload) {
    if !typing_ok(env) {
        return;
    }
    let Some(ep) = ep else {
        return;
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return;
    };
    let mid = typing_message_id(env);
    if cached_reaction(&mid).is_some() {
        return;
    }
    let base = open_base(ep);
    let Ok(http) = crate::llm_http::env_aware_client(8, base) else {
        return;
    };
    let Ok(token) = tenant_token(&http, base, &app_id, &secret).await else {
        return;
    };
    let url = typing_create_url(base, &mid);
    let Ok(resp) = http
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&typing_create_body())
        .send()
        .await
    else {
        return;
    };
    let Ok(data) = resp.json::<Value>().await else {
        return;
    };
    if !feishu_api_ok(&data) {
        return;
    }
    if let Some(rid) = parse_reaction_id(&data) {
        store_reaction(&mid, rid);
    }
}

pub(crate) async fn stop_typing(ep: Option<&ChannelEndpoint>, env: &NativePayload) {
    let Some(ep) = ep else {
        return;
    };
    let mid = typing_message_id(env);
    if mid.is_empty() {
        return;
    }
    let Some(rid) = take_reaction(&mid) else {
        return;
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return;
    };
    let base = open_base(ep);
    let Ok(http) = crate::llm_http::env_aware_client(8, base) else {
        return;
    };
    let Ok(token) = tenant_token(&http, base, &app_id, &secret).await else {
        return;
    };
    let url = typing_delete_url(base, &mid, &rid);
    let _ = http
        .delete(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await;
}

async fn run_once(
    http: &reqwest::Client,
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    app_id: &str,
    secret: &str,
    base: &str,
) -> Result<()> {
    let url = ws_endpoint(http, base, app_id, secret).await?;
    let service_id = query_i32(&url, "service_id");
    let host = url.split('?').next().unwrap_or("wss");
    eprintln!("hyper feishu connecting {host}");
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), connect_async(&url))
        .await
        .map_err(|_| Error::msg("feishu ws connect timeout"))?
        .map_err(|e| {
            let msg = e.to_string();
            let safe = msg.split('?').next().unwrap_or("ws error");
            Error::msg(format!("feishu ws connect: {safe}"))
        })?;
    feishu_trace(format!("hyper feishu connected {host}"));
    let api = FeishuApi {
        http: http.clone(),
        app_id: app_id.to_string(),
        secret: secret.to_string(),
        base: base.to_string(),
    };
    {
        let ep = ep.clone();
        let mgr = mgr.clone();
        let api = api.clone();
        tokio::spawn(async move {
            if let Err(e) = catchup_missed(&ep, &mgr, &api).await {
                feishu_trace(format!("hyper feishu catchup: {e}"));
            }
        });
    }
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));
    let ping_w = write.clone();
    let ping = tokio::spawn(async move {
        let mut tick = tokio::time::interval(PING_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut w = ping_w.lock().await;
            let pb = PbFrame::ping(service_id).encode();
            if w.send(Message::Ping(Vec::new().into())).await.is_err() {
                break;
            }
            if w.send(Message::Binary(pb.into())).await.is_err() {
                break;
            }
        }
    });
    let mut frags: HashMap<String, Vec<Option<Vec<u8>>>> = HashMap::new();
    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("feishu ws: {e}")))?;
        match frame {
            Message::Ping(p) => {
                let _ = write.lock().await.send(Message::Pong(p)).await;
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Text(text) => {
                handle_text(ep, mgr, &api, &write, &text).await;
            }
            Message::Binary(bin) => {
                handle_binary(ep, mgr, &api, &write, &bin, &mut frags).await;
            }
            _ => {}
        }
    }
    ping.abort();
    Ok(())
}

async fn handle_text(
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    api: &FeishuApi,
    write: &Arc<Mutex<WsWrite>>,
    text: &str,
) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        eprintln!("hyper feishu: non-json frame, skipping");
        return;
    };
    if is_ping(&v) {
        let _ = write
            .lock()
            .await
            .send(Message::Text(json!({"type": "pong"}).to_string().into()))
            .await;
        return;
    }
    if let Some(ack) = json_ack(&v) {
        let _ = write.lock().await.send(Message::Text(ack.into())).await;
    }
    spawn_ingest(ep, mgr, api, v);
}

async fn handle_binary(
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    api: &FeishuApi,
    write: &Arc<Mutex<WsWrite>>,
    bin: &[u8],
    frags: &mut HashMap<String, Vec<Option<Vec<u8>>>>,
) {
    if bin.first().copied() == Some(b'{') {
        if let Ok(text) = std::str::from_utf8(bin) {
            handle_text(ep, mgr, api, write, text).await;
            return;
        }
    }
    let Some(frame) = PbFrame::decode(bin) else {
        eprintln!("hyper feishu: non-json frame, skipping");
        return;
    };
    let kind = frame.header("type");
    if frame.method == PB_CONTROL || kind.eq_ignore_ascii_case("ping") {
        if kind.eq_ignore_ascii_case("ping") {
            let _ = write
                .lock()
                .await
                .send(Message::Binary(frame.to_pong().encode().into()))
                .await;
        }
        return;
    }
    if frame.method == PB_DATA || kind.eq_ignore_ascii_case("event") {
        let _ = write
            .lock()
            .await
            .send(Message::Binary(frame.to_ack().encode().into()))
            .await;
        let Some(payload) = assemble(&frame, frags) else {
            return;
        };
        match serde_json::from_slice::<Value>(&payload) {
            Ok(v) => spawn_ingest(ep, mgr, api, v),
            Err(_) => eprintln!("hyper feishu: non-json frame, skipping"),
        }
    }
}

fn spawn_ingest(ep: &ChannelEndpoint, mgr: &ChannelManager, api: &FeishuApi, v: Value) {
    let ep = ep.clone();
    let mgr = mgr.clone();
    let api = api.clone();
    tokio::spawn(async move {
        ingest_json(&ep, &mgr, &api, &v).await;
    });
}

async fn ingest_json(ep: &ChannelEndpoint, mgr: &ChannelManager, api: &FeishuApi, v: &Value) {
    if let Some(chat_id) = p2p_entered_chat_id(v) {
        remember_p2p_chat(&chat_id);
    }
    if let Some(mut env) = native_from_envelope(ep, v) {
        super::stamp_endpoint(&mut env, ep);
        remember_from_env(&env);
        if !claim_inbound(&js_str(env.meta.get("message_id").unwrap_or(&Value::Null))) {
            return;
        }
        feishu_trace(format!(
            "hyper feishu inbound group={} sender={}",
            env.is_group(),
            env.sender_id
        ));
        enrich_feishu_media(api, &mut env).await;
        if let Err(e) = mgr.ingest(env).await {
            feishu_trace(format!("hyper feishu ingest: {e}"));
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct CatchupStore {
    chats: HashMap<String, ChatCursor>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ChatCursor {
    last_id: String,
    #[serde(default)]
    last_time: u64,
    #[serde(default)]
    is_group: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum CatchupDecide {
    InitCursor,
    Skip,
    Ingest,
}

static CATCHUP: StdMutex<Option<CatchupStore>> = StdMutex::new(None);

struct InboundSeen {
    set: HashSet<String>,
    order: VecDeque<String>,
}

static INBOUND_SEEN: StdMutex<Option<InboundSeen>> = StdMutex::new(None);

/// First caller for this Feishu `message_id` wins. Empty ids are not gated.
fn claim_inbound(message_id: &str) -> bool {
    if message_id.is_empty() {
        return true;
    }
    let Ok(mut g) = INBOUND_SEEN.lock() else {
        return true;
    };
    let seen = g.get_or_insert_with(|| InboundSeen {
        set: HashSet::new(),
        order: VecDeque::new(),
    });
    if !seen.set.insert(message_id.to_string()) {
        return false;
    }
    seen.order.push_back(message_id.to_string());
    while seen.order.len() > INBOUND_SEEN_CAP {
        if let Some(old) = seen.order.pop_front() {
            seen.set.remove(&old);
        }
    }
    true
}

fn catchup_path() -> Option<PathBuf> {
    crate::config::Config::home_dir()
        .ok()
        .map(|h| h.join("channels").join("feishu.catchup.json"))
}

fn load_catchup() -> CatchupStore {
    let Some(p) = catchup_path() else {
        return CatchupStore::default();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_catchup(store: &CatchupStore) {
    let Some(p) = catchup_path() else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string(store) {
        let _ = std::fs::write(p, s);
    }
}

fn with_catchup<R>(f: impl FnOnce(&mut CatchupStore) -> R) -> Option<R> {
    let mut g = CATCHUP.lock().ok()?;
    if g.is_none() {
        *g = Some(load_catchup());
    }
    let store = g.as_mut()?;
    let r = f(store);
    save_catchup(store);
    Some(r)
}

fn parse_time_ms(raw: &str) -> u64 {
    let n: u64 = raw.trim().parse().unwrap_or(0);
    if n == 0 {
        return 0;
    }
    if n < 1_000_000_000_000 {
        n.saturating_mul(1000)
    } else {
        n
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn catchup_decide(
    cursor: Option<&ChatCursor>,
    id: &str,
    time_ms: u64,
    now_ms: u64,
) -> CatchupDecide {
    if id.is_empty() {
        return CatchupDecide::Skip;
    }
    let Some(c) = cursor else {
        return CatchupDecide::InitCursor;
    };
    if id == c.last_id {
        return CatchupDecide::Skip;
    }
    if time_ms > 0 && c.last_time > 0 && time_ms <= c.last_time {
        return CatchupDecide::Skip;
    }
    if time_ms > 0 && now_ms > time_ms && now_ms.saturating_sub(time_ms) > CATCHUP_MAX_AGE_MS {
        return CatchupDecide::Skip;
    }
    CatchupDecide::Ingest
}

fn remember_seen(chat_id: &str, is_group: bool, message_id: &str, create_time: &str) {
    if chat_id.is_empty() || message_id.is_empty() {
        return;
    }
    let t = parse_time_ms(create_time);
    let _ = with_catchup(|store| {
        let e = store
            .chats
            .entry(chat_id.to_string())
            .or_insert(ChatCursor {
                last_id: message_id.to_string(),
                last_time: t,
                is_group,
            });
        if t >= e.last_time {
            e.last_id = message_id.to_string();
            e.last_time = t;
            e.is_group = is_group;
        }
    });
}

fn remember_from_env(env: &NativePayload) {
    let chat_id = js_str(env.meta.get("chat_id").unwrap_or(&Value::Null));
    let mid = js_str(env.meta.get("message_id").unwrap_or(&Value::Null));
    let is_group = env
        .meta
        .get("is_group")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let create_time = js_str(env.meta.get("create_time").unwrap_or(&Value::Null));
    remember_seen(&chat_id, is_group, &mid, &create_time);
}

fn list_item_to_event(item: &Value, is_group: bool) -> Option<Value> {
    let sender_type = js_str(&item["sender"]["sender_type"]);
    if sender_type == "app" || sender_type == "bot" {
        return None;
    }
    let id = js_str(&item["sender"]["id"]);
    if id.is_empty() {
        return None;
    }
    let chat_id = js_str(&item["chat_id"]);
    let message_id = js_str(&item["message_id"]);
    let msg_type = js_str(&item["msg_type"]);
    Some(json!({
        "sender": {
            "sender_id": { "open_id": id },
            "sender_type": sender_type
        },
        "message": {
            "message_id": message_id,
            "chat_id": chat_id,
            "chat_type": if is_group { "group" } else { "p2p" },
            "message_type": if msg_type.is_empty() { "text" } else { msg_type.as_str() },
            "content": item["body"]["content"].clone(),
            "create_time": js_str(&item["create_time"])
        }
    }))
}

fn catchup_floor_cursor(now_ms: u64, is_group: bool) -> ChatCursor {
    ChatCursor {
        last_id: String::new(),
        last_time: now_ms.saturating_sub(CATCHUP_MAX_AGE_MS),
        is_group,
    }
}

fn parse_chat_list_items(data: &Value) -> Vec<(String, bool)> {
    let Some(arr) = data
        .get("data")
        .and_then(|d| d.get("items"))
        .and_then(Value::as_array)
        .or_else(|| data.get("items").and_then(Value::as_array))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for it in arr {
        let chat_id = js_str(&it["chat_id"]);
        if chat_id.is_empty() {
            continue;
        }
        let mode = js_str(&it["chat_mode"]);
        let is_group = !mode.eq_ignore_ascii_case("p2p");
        out.push((chat_id, is_group));
    }
    out
}

fn parse_chat_list_next_page(data: &Value) -> Option<String> {
    let has_more = data
        .pointer("/data/has_more")
        .or_else(|| data.get("has_more"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !has_more {
        return None;
    }
    let token = data
        .pointer("/data/page_token")
        .or_else(|| data.get("page_token"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn list_chats_url(base: &str, page_token: Option<&str>) -> String {
    let mut url = format!(
        "{}/open-apis/im/v1/chats?page_size=100&user_id_type=open_id&sort_type=ByCreateTimeAsc",
        base.trim_end_matches('/')
    );
    if let Some(token) = page_token.map(str::trim).filter(|s| !s.is_empty()) {
        url.push_str("&page_token=");
        url.push_str(&urlencoding_seg(token));
    }
    url
}

/// List API is groups the bot is in — never p2p. Stored cursors keep DMs.
fn merge_catchup_targets(
    stored: HashMap<String, ChatCursor>,
    listed: &[(String, bool)],
    now_ms: u64,
) -> HashMap<String, ChatCursor> {
    let mut chats = stored;
    for (chat_id, is_group) in listed {
        chats
            .entry(chat_id.clone())
            .or_insert_with(|| catchup_floor_cursor(now_ms, *is_group));
    }
    chats
}

fn remember_p2p_chat(chat_id: &str) {
    if chat_id.is_empty() {
        return;
    }
    let now_ms = unix_ms();
    let _ = with_catchup(|store| {
        store
            .chats
            .entry(chat_id.to_string())
            .or_insert_with(|| catchup_floor_cursor(now_ms, false));
    });
}

fn p2p_entered_chat_id(v: &Value) -> Option<String> {
    let et = envelope_event_type(v).to_ascii_lowercase();
    if !et.contains("p2p_chat_entered") {
        return None;
    }
    let chat_id = first_str(&[
        &v["event"]["chat_id"],
        &v["event"]["chat"]["chat_id"],
        &v["chat_id"],
    ]);
    if chat_id.is_empty() {
        None
    } else {
        Some(chat_id)
    }
}

async fn list_bot_chats(api: &FeishuApi, token: &str) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..20 {
        let url = list_chats_url(&api.base, page_token.as_deref());
        let resp = api
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await?;
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            if out.is_empty() {
                return Err(Error::msg(format!(
                    "feishu list chats HTTP {status} msg={}",
                    js_str(&data["msg"])
                )));
            }
            break;
        }
        if let Some(n) = data.get("code").and_then(Value::as_i64) {
            if n != 0 {
                if out.is_empty() {
                    return Err(Error::msg(format!(
                        "feishu list chats code={n} msg={}",
                        js_str(&data["msg"])
                    )));
                }
                break;
            }
        }
        out.extend(parse_chat_list_items(&data));
        match parse_chat_list_next_page(&data) {
            Some(next) => page_token = Some(next),
            None => break,
        }
    }
    Ok(out)
}

async fn catchup_missed(ep: &ChannelEndpoint, mgr: &ChannelManager, api: &FeishuApi) -> Result<()> {
    let token = match tenant_token(&api.http, &api.base, &api.app_id, &api.secret).await {
        Ok(t) => t,
        Err(e) => {
            feishu_trace(format!("hyper feishu catchup token: {e}"));
            return Ok(());
        }
    };
    let now_ms = unix_ms();
    let stored = with_catchup(|s| s.chats.clone()).unwrap_or_default();
    let listed = match list_bot_chats(api, &token).await {
        Ok(listed) => listed,
        Err(e) => {
            feishu_trace(format!("hyper feishu catchup list chats: {e}"));
            Vec::new()
        }
    };
    let chats = merge_catchup_targets(stored, &listed, now_ms);
    let _ = with_catchup(|s| {
        for (id, cur) in &chats {
            s.chats.entry(id.clone()).or_insert_with(|| cur.clone());
        }
    });
    if chats.is_empty() {
        feishu_trace("hyper feishu catchup idle (no stored chats; p2p seeds on inbound)");
        return Ok(());
    }
    feishu_trace(format!(
        "hyper feishu catchup {} chats ({} listed groups)",
        chats.len(),
        listed.len()
    ));
    for (chat_id, cursor) in chats {
        if let Err(e) = catchup_chat(ep, mgr, api, &token, &chat_id, &cursor, now_ms).await {
            feishu_trace(format!("hyper feishu catchup {chat_id}: {e}"));
        }
    }
    Ok(())
}

async fn catchup_chat(
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    api: &FeishuApi,
    token: &str,
    chat_id: &str,
    cursor: &ChatCursor,
    now_ms: u64,
) -> Result<()> {
    let items = list_chat_messages(api, token, chat_id).await?;
    let Some(newest) = items.first() else {
        return Ok(());
    };
    let newest_id = js_str(&newest["message_id"]);
    let newest_time = parse_time_ms(&js_str(&newest["create_time"]));
    if catchup_decide(Some(cursor), &newest_id, newest_time, now_ms) == CatchupDecide::Skip
        && items.iter().all(|it| {
            let id = js_str(&it["message_id"]);
            catchup_decide(
                Some(cursor),
                &id,
                parse_time_ms(&js_str(&it["create_time"])),
                now_ms,
            ) != CatchupDecide::Ingest
        })
    {
        remember_seen(
            chat_id,
            cursor.is_group,
            &newest_id,
            &js_str(&newest["create_time"]),
        );
        return Ok(());
    }
    let mut replay = Vec::new();
    for item in items.iter().rev() {
        let id = js_str(&item["message_id"]);
        let t = parse_time_ms(&js_str(&item["create_time"]));
        if catchup_decide(Some(cursor), &id, t, now_ms) != CatchupDecide::Ingest {
            continue;
        }
        let Some(event) = list_item_to_event(item, cursor.is_group) else {
            continue;
        };
        let Some(mut env) = native_from_event(ep, &event) else {
            continue;
        };
        super::stamp_endpoint(&mut env, ep);
        if !claim_inbound(&id) {
            continue;
        }
        replay.push(env);
    }
    for env in replay {
        feishu_trace(format!(
            "hyper feishu catchup inbound group={} sender={}",
            env.is_group(),
            env.sender_id
        ));
        if let Err(e) = mgr.ingest(env).await {
            feishu_trace(format!("hyper feishu catchup ingest: {e}"));
        }
    }
    remember_seen(
        chat_id,
        cursor.is_group,
        &newest_id,
        &js_str(&newest["create_time"]),
    );
    Ok(())
}

async fn list_chat_messages(api: &FeishuApi, token: &str, chat_id: &str) -> Result<Vec<Value>> {
    let url = list_messages_url(&api.base, chat_id);
    let resp = api
        .http
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(Error::msg(format!(
            "feishu list messages HTTP {status} msg={}",
            js_str(&data["msg"])
        )));
    }
    if let Some(n) = data.get("code").and_then(Value::as_i64) {
        if n != 0 {
            return Err(Error::msg(format!(
                "feishu list messages code={n} msg={}",
                js_str(&data["msg"])
            )));
        }
    }
    Ok(data["data"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn list_messages_url(base: &str, chat_id: &str) -> String {
    format!(
        "{}/open-apis/im/v1/messages?container_id_type=chat&container_id={}&sort_type=ByCreateTimeDesc&page_size=20",
        base.trim_end_matches('/'),
        urlencoding_seg(chat_id)
    )
}

fn json_ack(v: &Value) -> Option<String> {
    let mid = header_str(v, "message_id");
    let need = boolish(&v["need_ack"]) || boolish(&v["headers"]["need_ack"]) || !mid.is_empty();
    if !need {
        return None;
    }
    Some(json!({"type": "ack", "code": 200, "message_id": mid}).to_string())
}

async fn ws_endpoint(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
) -> Result<String> {
    // Official lark-oapi path. Bearer + empty body on this URL is 404/9499.
    let url = format!("{base}/callback/ws/endpoint");
    post_ws_url(http, &url, json!({"AppID": app_id, "AppSecret": secret})).await
}

async fn post_ws_url(http: &reqwest::Client, url: &str, body: Value) -> Result<String> {
    let resp = http
        .post(url)
        .header("locale", "zh")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = data.get("code").cloned().unwrap_or(Value::Null);
        let msg = js_str(&data["msg"]);
        return Err(Error::msg(format!(
            "feishu ws endpoint HTTP {status} code={code} msg={msg}"
        )));
    }
    if let Some(n) = data.get("code").and_then(Value::as_i64) {
        if n != 0 {
            return Err(Error::msg(format!(
                "feishu ws endpoint code={n} msg={}",
                js_str(&data["msg"])
            )));
        }
    }
    pick_ws_url(&data).ok_or_else(|| Error::msg("feishu ws endpoint: no url"))
}

fn pick_ws_url(data: &Value) -> Option<String> {
    for v in [
        &data["data"]["url"],
        &data["data"]["URL"],
        &data["url"],
        &data["URL"],
        &data["data"]["ws_url"],
    ] {
        let s = js_str(v);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

async fn tenant_token(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
) -> Result<String> {
    if let Ok(g) = TOKEN.lock() {
        if let Some(c) = g.as_ref() {
            if c.app_id == app_id && c.base == base && Instant::now() < c.until {
                return Ok(c.token.clone());
            }
        }
    }
    let url = format!("{base}/open-apis/auth/v3/tenant_access_token/internal");
    let data: Value = http
        .post(url)
        .json(&json!({"app_id": app_id, "app_secret": secret}))
        .send()
        .await?
        .json()
        .await?;
    let token = js_str(&data["tenant_access_token"]);
    if token.is_empty() {
        return Err(Error::msg(format!(
            "feishu token code={} msg={}",
            data.get("code").unwrap_or(&Value::Null),
            js_str(&data["msg"])
        )));
    }
    if let Ok(mut g) = TOKEN.lock() {
        *g = Some(CachedToken {
            app_id: app_id.to_string(),
            base: base.to_string(),
            token: token.clone(),
            until: Instant::now() + TOKEN_TTL,
        });
    }
    Ok(token)
}

fn clear_token(app_id: &str) {
    if let Ok(mut g) = TOKEN.lock() {
        if g.as_ref().is_some_and(|c| c.app_id == app_id) {
            *g = None;
        }
    }
}

async fn send_text(
    http: &reqwest::Client,
    env: &NativePayload,
    base: &str,
    app_id: &str,
    secret: &str,
    text: &str,
) -> Result<String> {
    match send_im(
        http,
        env,
        base,
        app_id,
        secret,
        "post",
        super::im_md::feishu_post(text),
    )
    .await
    {
        Ok(id) => Ok(id),
        Err(e) if is_feishu_illegal_content(&e) => {
            feishu_trace(format!("hyper feishu post illegal, fallback text: {e}"));
            let mut env2 = env.clone();
            env2.meta.remove("delivery_id");
            send_im(
                http,
                &env2,
                base,
                app_id,
                secret,
                "text",
                feishu_plain_text(text),
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// Feishu text messages allow 20 PATCH edits (`230072`). Rotate before the
/// cap so a long tool loop does not freeze or spam new bubbles.
const FEISHU_EDIT_CAP: u32 = 20;
const FEISHU_ROTATE_AT: u32 = 18;
/// One Feishu bubble in a final reply (Hermes batches at 4000 chars).
const FEISHU_TEXT_BUBBLE: usize = 4000;

struct BubbleSlot {
    mid: String,
    patches: u32,
}

fn is_feishu_edit_exhausted(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("230072") || s.contains("230075") || s.contains("number of times it can be edited")
}

fn is_feishu_illegal_content(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("230001") || s.contains("content is illegal") || s.contains("invalid content")
}

fn feishu_plain_text(src: &str) -> serde_json::Value {
    let t = super::im_md::separated_plain(src);
    let t: String = if t.chars().count() > 8000 {
        t.chars().take(8000).collect()
    } else {
        t
    };
    serde_json::json!({ "text": t })
}

fn should_rotate_feishu_bubble(patches: u32) -> bool {
    patches >= FEISHU_ROTATE_AT
}

async fn promote_or_send_text(
    http: &reqwest::Client,
    env: &NativePayload,
    base: &str,
    app_id: &str,
    secret: &str,
    text: &str,
) -> Result<()> {
    // Final replies are a new message. Patching the progress bubble hides the
    // answer at the bottom of the chat after a long Ask wait. Long answers
    // split into bubbles (Hermes smart chunking) so the plain-text fallback
    // never has to clip the tail.
    take_bubble(&env.progress_bubble_key());
    for chunk in super::chunk::chunk_text(text, FEISHU_TEXT_BUBBLE) {
        send_text(http, env, base, app_id, secret, &chunk).await?;
    }
    Ok(())
}

async fn upsert_progress_text(
    http: &reqwest::Client,
    env: &NativePayload,
    base: &str,
    app_id: &str,
    secret: &str,
    text: &str,
) -> Result<()> {
    let key = env.progress_bubble_key();
    if let Some(slot) = peek_slot(&key) {
        if !should_rotate_feishu_bubble(slot.patches) {
            match patch_text(http, base, app_id, secret, &slot.mid, text).await {
                Ok(()) => {
                    bump_bubble(&key);
                    return Ok(());
                }
                Err(e) if is_feishu_edit_exhausted(&e) => {
                    feishu_trace(format!("hyper feishu patch: {e}"));
                }
                Err(e) => {
                    feishu_trace(format!("hyper feishu patch: {e}"));
                    return Err(e);
                }
            }
        }
    }
    let mid = send_text(http, env, base, app_id, secret, text).await?;
    if !mid.is_empty() {
        store_bubble(&key, mid);
    }
    Ok(())
}

fn bubbles() -> &'static StdMutex<HashMap<String, BubbleSlot>> {
    static C: OnceLock<StdMutex<HashMap<String, BubbleSlot>>> = OnceLock::new();
    C.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn store_bubble(key: &str, mid: String) {
    let Ok(mut g) = bubbles().lock() else {
        return;
    };
    g.insert(key.to_string(), BubbleSlot { mid, patches: 0 });
}

fn peek_slot(key: &str) -> Option<BubbleSlot> {
    let Ok(g) = bubbles().lock() else {
        return None;
    };
    g.get(key).map(|s| BubbleSlot {
        mid: s.mid.clone(),
        patches: s.patches,
    })
}

fn bump_bubble(key: &str) {
    let Ok(mut g) = bubbles().lock() else {
        return;
    };
    if let Some(s) = g.get_mut(key) {
        s.patches = s.patches.saturating_add(1).min(FEISHU_EDIT_CAP);
    }
}

fn take_bubble(key: &str) -> Option<String> {
    let Ok(mut g) = bubbles().lock() else {
        return None;
    };
    g.remove(key).map(|s| s.mid)
}

fn parse_sent_message_id(data: &Value) -> Option<String> {
    let id = js_str(&data["data"]["message_id"]);
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn sent_chat_meta(data: &Value) -> Option<(String, String, String)> {
    let chat_id = js_str(&data["data"]["chat_id"]);
    if chat_id.is_empty() {
        return None;
    }
    Some((
        chat_id,
        js_str(&data["data"]["message_id"]),
        js_str(&data["data"]["create_time"]),
    ))
}

fn remember_from_send(env: &NativePayload, data: &Value) {
    if let Some((chat_id, mid, create_time)) = sent_chat_meta(data) {
        remember_seen(&chat_id, env.is_group(), &mid, &create_time);
        return;
    }
    remember_from_env(env);
}

fn patch_url(base: &str, message_id: &str) -> String {
    format!(
        "{base}/open-apis/im/v1/messages/{}",
        urlencoding_seg(message_id)
    )
}

async fn patch_text(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
    message_id: &str,
    text: &str,
) -> Result<()> {
    let content =
        serde_json::to_string(&super::im_md::feishu_post(text)).unwrap_or_else(|_| "{}".into());
    let body = json!({
        "msg_type": "post",
        "content": content,
    });
    let token = tenant_token(http, base, app_id, secret).await?;
    let url = patch_url(base, message_id);
    let resp = http
        .put(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    let code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if status.is_success() && code == 0 {
        return Ok(());
    }
    if status.as_u16() == 401 || code == 99991663 {
        clear_token(app_id);
    }
    let err = Error::msg(format!(
        "feishu patch HTTP {status} code={code} msg={}",
        js_str(&data["msg"])
    ));
    if is_feishu_illegal_content(&err) {
        let content =
            serde_json::to_string(&feishu_plain_text(text)).unwrap_or_else(|_| "{}".into());
        let body = json!({
            "msg_type": "text",
            "content": content,
        });
        let token = tenant_token(http, base, app_id, secret).await?;
        let resp = http
            .put(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        let code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if status.is_success() && code == 0 {
            return Ok(());
        }
        return Err(Error::msg(format!(
            "feishu patch HTTP {status} code={code} msg={}",
            js_str(&data["msg"])
        )));
    }
    Err(err)
}

async fn send_media(
    http: &reqwest::Client,
    env: &NativePayload,
    base: &str,
    app_id: &str,
    secret: &str,
    part: &ContentPart,
) -> Result<()> {
    let blob = super::xfer::load_part(part, None).await?;
    match blob.kind {
        super::xfer::Kind::Image => {
            let key = upload_image(http, base, app_id, secret, &blob).await?;
            send_im(
                http,
                env,
                base,
                app_id,
                secret,
                "image",
                json!({"image_key": key}),
            )
            .await
            .map(|_| ())
        }
        super::xfer::Kind::Audio => {
            let key = upload_file(http, base, app_id, secret, &blob, "stream").await?;
            send_im(
                http,
                env,
                base,
                app_id,
                secret,
                "audio",
                json!({"file_key": key}),
            )
            .await
            .map(|_| ())
        }
        super::xfer::Kind::Video => {
            let key = upload_file(http, base, app_id, secret, &blob, "mp4").await?;
            send_im(
                http,
                env,
                base,
                app_id,
                secret,
                "media",
                json!({"file_key": key}),
            )
            .await
            .map(|_| ())
        }
        super::xfer::Kind::File => {
            let ft = feishu_file_type(&blob.name);
            let key = upload_file(http, base, app_id, secret, &blob, ft).await?;
            send_im(
                http,
                env,
                base,
                app_id,
                secret,
                "file",
                json!({"file_key": key}),
            )
            .await
            .map(|_| ())
        }
    }
}

fn feishu_file_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "opus" => "opus",
        "mp4" => "mp4",
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" => "xls",
        "ppt" | "pptx" => "ppt",
        _ => "stream",
    }
}

async fn upload_image(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
    blob: &super::xfer::Blob,
) -> Result<String> {
    let form = reqwest::multipart::Form::new()
        .text("image_type", "message")
        .part("image", super::xfer::bytes_part(blob));
    upload_form(
        http,
        base,
        app_id,
        secret,
        &format!("{base}/open-apis/im/v1/images"),
        form,
        "image_key",
    )
    .await
}

async fn upload_file(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
    blob: &super::xfer::Blob,
    file_type: &str,
) -> Result<String> {
    let form = reqwest::multipart::Form::new()
        .text("file_type", file_type.to_string())
        .text("file_name", blob.name.clone())
        .part("file", super::xfer::bytes_part(blob));
    upload_form(
        http,
        base,
        app_id,
        secret,
        &format!("{base}/open-apis/im/v1/files"),
        form,
        "file_key",
    )
    .await
}

async fn upload_form(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
    url: &str,
    form: reqwest::multipart::Form,
    key_field: &str,
) -> Result<String> {
    let token = tenant_token(http, base, app_id, secret).await?;
    let resp = http
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    let code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if status.is_success() && code == 0 {
        let key = js_str(&data["data"][key_field]);
        if key.is_empty() {
            return Err(Error::msg(format!("feishu upload: missing {key_field}")));
        }
        return Ok(key);
    }
    if status.as_u16() == 401 || code == 99991663 {
        clear_token(app_id);
    }
    Err(Error::msg(format!(
        "feishu upload HTTP {status} code={code} msg={}",
        js_str(&data["msg"])
    )))
}

async fn send_im(
    http: &reqwest::Client,
    env: &NativePayload,
    base: &str,
    app_id: &str,
    secret: &str,
    msg_type: &str,
    content: Value,
) -> Result<String> {
    let content = serde_json::to_string(&content).unwrap_or_else(|_| "{}".into());
    let id_type = receive_id_type(env);
    let url = send_create_url(base, env, &id_type);
    let stable_uuid = env
        .meta
        .get("delivery_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let body = if send_is_reply(&url) {
        json!({
            "msg_type": msg_type,
            "content": content,
            "uuid": stable_uuid,
        })
    } else {
        let receive_id = receive_id(env, &id_type);
        if receive_id.is_empty() {
            return Err(Error::msg("feishu send: missing chat_id / open_id"));
        }
        json!({
            "receive_id": receive_id,
            "msg_type": msg_type,
            "content": content,
            "uuid": stable_uuid,
        })
    };
    let mut last = Error::msg("feishu send failed");
    for _ in 0..2 {
        let token = tenant_token(http, base, app_id, secret).await?;
        let resp = match http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.into();
                continue;
            }
        };
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        let code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if status.is_success() && code == 0 {
            remember_from_send(env, &data);
            return Ok(parse_sent_message_id(&data).unwrap_or_default());
        }
        if status.as_u16() == 401 || code == 99991663 {
            clear_token(app_id);
            last = Error::msg(format!("feishu send unauthorized code={code}"));
            continue;
        }
        last = Error::msg(format!(
            "feishu send HTTP {status} code={code} msg={}",
            js_str(&data["msg"])
        ));
        break;
    }
    Err(last)
}

fn send_create_url(base: &str, env: &NativePayload, id_type: &str) -> String {
    let reply = typing_message_id(env);
    if reply.is_empty() {
        format!("{base}/open-apis/im/v1/messages?receive_id_type={id_type}")
    } else {
        format!(
            "{base}/open-apis/im/v1/messages/{}/reply",
            urlencoding_seg(&reply)
        )
    }
}

fn send_is_reply(url: &str) -> bool {
    url.contains("/reply")
}

fn receive_id_type(env: &NativePayload) -> String {
    let t = js_str(env.meta.get("receive_id_type").unwrap_or(&Value::Null));
    if !t.is_empty() {
        return t;
    }
    if env.is_group() {
        "chat_id".into()
    } else {
        "open_id".into()
    }
}

fn receive_id(env: &NativePayload, id_type: &str) -> String {
    if id_type == "open_id" {
        let id = js_str(env.meta.get("receive_id").unwrap_or(&Value::Null));
        if !id.is_empty() {
            return id;
        }
        return env.sender_id.clone();
    }
    env.chat_id()
}

fn native_from_envelope(ep: &ChannelEndpoint, v: &Value) -> Option<NativePayload> {
    if is_ping(v) {
        return None;
    }
    let et = envelope_event_type(v);
    let et_l = et.to_ascii_lowercase();
    if is_card_action(&et_l) {
        return native_from_card_action(ep, v);
    }
    if et_l.contains("card") {
        return None;
    }
    let event = extract_event(v)?;
    native_from_event(ep, &event)
}

fn is_card_action(event_type: &str) -> bool {
    event_type == "card.action.trigger"
        || event_type.ends_with("card.action.trigger")
        || event_type.contains("card.action.trigger")
}

fn native_from_card_action(ep: &ChannelEndpoint, v: &Value) -> Option<NativePayload> {
    let event = v.get("event").unwrap_or(v);
    let choice = card_choice(event);
    if choice.is_empty() {
        return None;
    }
    let open_id = first_str(&[
        &event["operator"]["open_id"],
        &event["operator"]["user_id"],
        &event["operator"]["sender_id"]["open_id"],
    ]);
    if open_id.is_empty() {
        return None;
    }
    let chat_id = first_str(&[
        &event["context"]["open_chat_id"],
        &event["context"]["chat_id"],
        &event["message"]["chat_id"],
        &event["open_chat_id"],
    ]);
    let message_id = first_str(&[
        &v["header"]["event_id"],
        &event["context"]["open_message_id"],
        &event["open_message_id"],
    ]);
    let chat_type = first_str(&[
        &event["context"]["chat_type"],
        &event["message"]["chat_type"],
        &event["host"],
    ]);
    let is_group = chat_type.to_ascii_lowercase().contains("group");
    let receive_id_type = if is_group { "chat_id" } else { "open_id" };
    let receive_id = if is_group {
        chat_id.clone()
    } else {
        open_id.clone()
    };
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "feishu".into()
        } else {
            ep.kind.clone()
        },
        sender_id: open_id,
        sender_name: first_str(&[&event["operator"]["name"]]),
        content_parts: vec![ContentPart::text(&choice)],
        text: choice,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("is_mentioned".into(), json!(true));
    env.mark_choice_click();
    if !message_id.is_empty() {
        env.meta.insert("message_id".into(), json!(message_id));
    }
    env.meta
        .insert("receive_id_type".into(), json!(receive_id_type));
    env.meta.insert("receive_id".into(), json!(receive_id));
    Some(env)
}

fn card_choice(event: &Value) -> String {
    let value = &event["action"]["value"];
    let direct = first_str(&[
        &value["choice"],
        &value["id"],
        &value["option"],
        &event["action"]["option"],
    ]);
    if !direct.is_empty() {
        return direct;
    }
    match value {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .values()
            .find_map(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn native_from_event(ep: &ChannelEndpoint, event: &Value) -> Option<NativePayload> {
    let message = &event["message"];
    let sender = &event["sender"];
    if message.is_null() {
        return None;
    }
    let sender_type = js_str(&sender["sender_type"]);
    if sender_type == "app" || sender_type == "bot" {
        return None;
    }
    let open_id = first_str(&[
        &sender["sender_id"]["open_id"],
        &sender["sender_id"]["user_id"],
        &sender["open_id"],
    ]);
    if open_id.is_empty() {
        return None;
    }
    let chat_id = js_str(&message["chat_id"]);
    let chat_type = js_str(&message["chat_type"]);
    let is_group = chat_type.eq_ignore_ascii_case("group");
    let msg_type = js_str(&message["message_type"]);
    let mut text = parse_text_content(&message["content"]);
    if text.is_empty() {
        text = feishu_nontext_caption(&msg_type, &message["content"]);
    }
    if text.is_empty() {
        return None;
    }
    let message_id = js_str(&message["message_id"]);
    let mentioned = !is_group || mentions_present(&message["mentions"]) || text.contains("@_all");
    let receive_id_type = if is_group { "chat_id" } else { "open_id" };
    let receive_id = if is_group {
        chat_id.clone()
    } else {
        open_id.clone()
    };
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "feishu".into()
        } else {
            ep.kind.clone()
        },
        sender_id: open_id,
        sender_name: js_str(&sender["name"]),
        content_parts: vec![ContentPart::text(&text)],
        text,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("message_id".into(), json!(message_id));
    let create_time = js_str(&message["create_time"]);
    if !create_time.is_empty() {
        env.meta.insert("create_time".into(), json!(create_time));
    }
    env.meta
        .insert("receive_id_type".into(), json!(receive_id_type));
    env.meta.insert("receive_id".into(), json!(receive_id));
    env.meta.insert("is_mentioned".into(), json!(mentioned));
    let obj = parse_content_obj(&message["content"]);
    let image_key = js_str(&obj["image_key"]);
    let file_key = js_str(&obj["file_key"]);
    if !image_key.is_empty() {
        env.meta
            .insert("image_key".into(), json!(image_key.clone()));
        env.meta.insert("resource_key".into(), json!(image_key));
        env.meta.insert("resource_type".into(), json!("image"));
    } else if !file_key.is_empty() {
        env.meta.insert("file_key".into(), json!(file_key.clone()));
        env.meta.insert("resource_key".into(), json!(file_key));
        let ty = match msg_type.trim().to_ascii_lowercase().as_str() {
            "image" | "sticker" => "image",
            _ => "file",
        };
        env.meta.insert("resource_type".into(), json!(ty));
    }
    Some(env)
}

async fn enrich_feishu_media(api: &FeishuApi, env: &mut NativePayload) {
    let mid = js_str(env.meta.get("message_id").unwrap_or(&Value::Null));
    let key = js_str(env.meta.get("resource_key").unwrap_or(&Value::Null));
    if mid.is_empty() || key.is_empty() {
        return;
    }
    let ty = js_str(env.meta.get("resource_type").unwrap_or(&Value::Null));
    let ty = if ty.is_empty() { "file" } else { ty.as_str() };
    match download_resource(api, &mid, &key, ty).await {
        Ok(part) => {
            super::xfer::splice_media(&mut env.content_parts, part);
            env.text = super::xfer::query_text_of(&env.content_parts);
        }
        Err(e) => eprintln!("hyper feishu media: {e}"),
    }
}

async fn download_resource(
    api: &FeishuApi,
    message_id: &str,
    key: &str,
    ty: &str,
) -> Result<ContentPart> {
    let token = tenant_token(&api.http, &api.base, &api.app_id, &api.secret).await?;
    let url = format!(
        "{}/open-apis/im/v1/messages/{}/resources/{}?type={}",
        api.base,
        urlencoding_seg(message_id),
        urlencoding_seg(key),
        urlencoding_seg(ty)
    );
    let resp = api
        .http
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!(
            "feishu resource HTTP {}",
            resp.status()
        )));
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let name = disposition_name(resp.headers()).unwrap_or_else(|| {
        if ty == "image" {
            "image.jpg".into()
        } else {
            "file.bin".into()
        }
    });
    let bytes = resp.bytes().await?;
    if bytes.len() > super::xfer::FETCH_CAP {
        return Err(Error::msg("feishu resource over cap"));
    }
    let kind = super::xfer::kind_from_mime_name(&mime, &name);
    let mime = if mime.is_empty() {
        super::xfer::guess_mime(&name, kind).to_string()
    } else {
        mime
    };
    super::xfer::blob_to_inbound_part(super::xfer::Blob {
        kind,
        mime,
        name,
        bytes: bytes.to_vec(),
    })
}

fn urlencoding_seg(s: &str) -> String {
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

fn disposition_name(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let raw = headers
        .get(reqwest::header::CONTENT_DISPOSITION)?
        .to_str()
        .ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("filename*=") {
            let v = v.trim().trim_matches('"');
            if let Some((_, name)) = v.split_once("''") {
                return Some(name.to_string());
            }
        }
        if let Some(v) = part.strip_prefix("filename=") {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn parse_content_obj(content: &Value) -> Value {
    match content {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        Value::Object(_) => content.clone(),
        _ => Value::Null,
    }
}

#[cfg(test)]
fn parse_text_content_str(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return parse_text_content(&v);
    }
    raw.trim().to_string()
}

fn feishu_nontext_caption(msg_type: &str, content: &Value) -> String {
    let kind = msg_type.trim().to_ascii_lowercase();
    match kind.as_str() {
        "image" | "sticker" => "[图片]".into(),
        "audio" => "[语音]".into(),
        "media" | "video" => "[视频]".into(),
        "file" => {
            let name = parse_file_name(content);
            if name.is_empty() {
                "[文件]".into()
            } else {
                format!("[文件] {name}")
            }
        }
        _ => String::new(),
    }
}

fn parse_file_name(content: &Value) -> String {
    let from_obj = |v: &Value| {
        let n = js_str(&v["file_name"]);
        if n.is_empty() {
            js_str(&v["name"])
        } else {
            n
        }
    };
    match content {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .map(|v| from_obj(&v))
            .unwrap_or_default(),
        Value::Object(_) => from_obj(content),
        _ => String::new(),
    }
}

fn parse_text_content(content: &Value) -> String {
    match content {
        Value::String(s) => {
            if let Ok(v) = serde_json::from_str::<Value>(s) {
                let t = js_str(&v["text"]);
                if !t.is_empty() {
                    return t;
                }
                if v.is_object() {
                    return String::new();
                }
            }
            s.trim().to_string()
        }
        Value::Object(_) => js_str(&content["text"]),
        _ => String::new(),
    }
}

fn extract_event(v: &Value) -> Option<Value> {
    if looks_like_im_event(v) {
        return Some(v.clone());
    }
    for key in ["event", "data", "payload"] {
        match v.get(key) {
            Some(Value::String(s)) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    if let Some(ev) = extract_event(&parsed) {
                        return Some(ev);
                    }
                }
            }
            Some(inner) => {
                if let Some(ev) = extract_event(inner) {
                    return Some(ev);
                }
            }
            None => {}
        }
    }
    None
}

fn looks_like_im_event(v: &Value) -> bool {
    v.get("message").is_some() && (v.get("sender").is_some() || v.get("sender_id").is_some())
}

fn envelope_event_type(v: &Value) -> String {
    first_str(&[
        &v["header"]["event_type"],
        &v["event"]["header"]["event_type"],
        &v["event_type"],
        &v["type"],
    ])
}

fn is_ping(v: &Value) -> bool {
    let t = js_str(&v["type"]);
    t.eq_ignore_ascii_case("ping") || v.get("ping").is_some()
}

fn mentions_present(mentions: &Value) -> bool {
    mentions.as_array().is_some_and(|a| !a.is_empty())
}

fn extra(ep: &ChannelEndpoint, key: &str) -> String {
    ep.extra
        .get(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn first_str(vals: &[&Value]) -> String {
    for v in vals {
        let s = js_str(v);
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

fn boolish(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Value::Number(n) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn header_str(v: &Value, key: &str) -> String {
    let h = &v["headers"];
    if let Some(x) = h.get(key) {
        let s = js_str(x);
        if !s.is_empty() {
            return s;
        }
    }
    if let Some(arr) = h.as_array() {
        for item in arr {
            let k = first_str(&[&item["key"], &item["Key"]]);
            if k.eq_ignore_ascii_case(key) {
                return first_str(&[&item["value"], &item["Value"]]);
            }
        }
    }
    String::new()
}

fn query_i32(url: &str, key: &str) -> i32 {
    let Some(q) = url.split_once('?').map(|(_, q)| q) else {
        return 0;
    };
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return v.parse().unwrap_or(0);
            }
        }
    }
    0
}

fn assemble(frame: &PbFrame, frags: &mut HashMap<String, Vec<Option<Vec<u8>>>>) -> Option<Vec<u8>> {
    let sum: usize = frame.header("sum").parse().unwrap_or(1);
    let seq: usize = frame.header("seq").parse().unwrap_or(0);
    let msg_id = frame.header("message_id").to_string();
    if sum <= 1 || msg_id.is_empty() {
        return Some(frame.payload.clone());
    }
    let entry = frags
        .entry(msg_id.clone())
        .or_insert_with(|| vec![None; sum]);
    if entry.len() != sum {
        *entry = vec![None; sum];
    }
    if seq >= sum {
        return Some(frame.payload.clone());
    }
    entry[seq] = Some(frame.payload.clone());
    if entry.iter().all(|s| s.is_some()) {
        let full: Vec<u8> = entry
            .iter()
            .flat_map(|s| s.as_deref().unwrap_or(&[]))
            .copied()
            .collect();
        frags.remove(&msg_id);
        Some(full)
    } else {
        None
    }
}

fn decode_header(buf: &[u8]) -> Option<PbHeader> {
    let mut h = PbHeader::default();
    let mut i = 0usize;
    while i < buf.len() {
        let key = pb_varint(buf, &mut i)?;
        let field = (key >> 3) as u32;
        let wire = (key & 7) as u32;
        match (field, wire) {
            (1, 2) => h.key = pb_string(buf, &mut i)?,
            (2, 2) => h.value = pb_string(buf, &mut i)?,
            (_, w) => pb_skip(buf, &mut i, w)?,
        }
    }
    Some(h)
}

fn pb_u64(field: u32, v: u64, out: &mut Vec<u8>) {
    pb_put_varint(u64::from(field) << 3, out);
    pb_put_varint(v, out);
}

fn pb_i32(field: u32, v: i32, out: &mut Vec<u8>) {
    pb_u64(field, v as u64, out);
}

fn pb_bytes(field: u32, b: &[u8], out: &mut Vec<u8>) {
    pb_put_varint((u64::from(field) << 3) | 2, out);
    pb_put_varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

fn pb_put_varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
            out.push(b);
        } else {
            out.push(b);
            break;
        }
    }
}

fn pb_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    while *i < buf.len() {
        let b = buf[*i];
        *i += 1;
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn pb_len<'a>(buf: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    let n = pb_varint(buf, i)? as usize;
    if *i + n > buf.len() {
        return None;
    }
    let s = &buf[*i..*i + n];
    *i += n;
    Some(s)
}

fn pb_string(buf: &[u8], i: &mut usize) -> Option<String> {
    let b = pb_len(buf, i)?;
    Some(String::from_utf8_lossy(b).into_owned())
}

fn pb_skip(buf: &[u8], i: &mut usize, wire: u32) -> Option<()> {
    match wire {
        0 => {
            pb_varint(buf, i)?;
            Some(())
        }
        1 => {
            *i += 8;
            (*i <= buf.len()).then_some(())
        }
        2 => {
            pb_len(buf, i)?;
            Some(())
        }
        5 => {
            *i += 4;
            (*i <= buf.len()).then_some(())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ep_with(pairs: &[(&str, &str)]) -> ChannelEndpoint {
        let mut ep = ChannelEndpoint::default();
        ep.kind = "feishu".into();
        for (k, v) in pairs {
            ep.extra.insert((*k).into(), (*v).into());
        }
        ep
    }

    #[test]
    fn credentials_app_id_secret() {
        let ep = ep_with(&[("app_id", "cli_a"), ("app_secret", "s3cret")]);
        assert_eq!(credentials(&ep), Some(("cli_a".into(), "s3cret".into())));
    }

    #[test]
    fn credentials_qr_client_id_maps_to_app_id() {
        let ep = ep_with(&[("client_id", "cli_qr"), ("app_secret", "s")]);
        assert_eq!(credentials(&ep).unwrap().0, "cli_qr");
    }

    #[test]
    fn credentials_missing_secret() {
        let ep = ep_with(&[("app_id", "cli_a")]);
        assert!(credentials(&ep).is_none());
    }

    #[test]
    fn domain_defaults_feishu() {
        let ep = ep_with(&[]);
        assert_eq!(open_base(&ep), FEISHU_BASE);
    }

    #[test]
    fn domain_lark_from_domain() {
        let ep = ep_with(&[("domain", "lark")]);
        assert_eq!(open_base(&ep), LARK_BASE);
        let ep = ep_with(&[("domain", "larksuite")]);
        assert_eq!(open_base(&ep), LARK_BASE);
    }

    #[test]
    fn domain_lark_from_tenant_brand() {
        let ep = ep_with(&[("tenant_brand", "lark")]);
        assert_eq!(open_base(&ep), LARK_BASE);
        let ep = ep_with(&[("tenant_brand", "feishu")]);
        assert_eq!(open_base(&ep), FEISHU_BASE);
    }

    #[test]
    fn parse_text_content_json() {
        assert_eq!(parse_text_content_str(r#"{"text":"hello"}"#), "hello");
        assert_eq!(parse_text_content_str(""), "");
        assert_eq!(parse_text_content_str("plain"), "plain");
    }

    #[test]
    fn claim_inbound_drops_duplicate_ids() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("om_claim_{}_{}_{}", std::process::id(), unix_ms(), n);
        assert!(claim_inbound(&id));
        assert!(!claim_inbound(&id));
        assert!(claim_inbound(&format!("{id}_b")));
        assert!(claim_inbound(""));
        assert!(claim_inbound(""));
    }

    #[test]
    fn catchup_decide_skips_seen_stale_and_inits() {
        let cur = ChatCursor {
            last_id: "om_1".into(),
            last_time: 1_700_000_000_000,
            is_group: false,
        };
        let now = 1_700_000_000_000 + 60_000;
        assert_eq!(
            catchup_decide(None, "om_new", now, now),
            CatchupDecide::InitCursor
        );
        assert_eq!(
            catchup_decide(Some(&cur), "om_1", cur.last_time, now),
            CatchupDecide::Skip
        );
        assert_eq!(
            catchup_decide(Some(&cur), "om_old", cur.last_time - 1, now),
            CatchupDecide::Skip
        );
        assert_eq!(
            catchup_decide(Some(&cur), "om_ancient", now - CATCHUP_MAX_AGE_MS - 1, now),
            CatchupDecide::Skip
        );
        assert_eq!(
            catchup_decide(Some(&cur), "om_2", cur.last_time + 1, now),
            CatchupDecide::Ingest
        );
    }

    #[test]
    fn catchup_floor_replays_recent_only() {
        let now = 1_700_000_000_000 + CATCHUP_MAX_AGE_MS;
        let cur = catchup_floor_cursor(now, false);
        assert!(cur.last_id.is_empty());
        assert_eq!(
            catchup_decide(Some(&cur), "om_recent", now - 60_000, now),
            CatchupDecide::Ingest
        );
        assert_eq!(
            catchup_decide(Some(&cur), "om_old", cur.last_time.saturating_sub(1), now),
            CatchupDecide::Skip
        );
    }

    #[test]
    fn parse_chat_list_items_p2p_and_group() {
        let data = json!({
            "code": 0,
            "data": {
                "items": [
                    {"chat_id": "oc_dm", "chat_mode": "p2p"},
                    {"chat_id": "oc_g", "chat_mode": "group"},
                    {"chat_id": "", "chat_mode": "p2p"},
                    {"chat_mode": "p2p"}
                ]
            }
        });
        assert_eq!(
            parse_chat_list_items(&data),
            vec![("oc_dm".into(), false), ("oc_g".into(), true)]
        );
        assert_eq!(parse_chat_list_next_page(&data), None);
        let paged = json!({
            "code": 0,
            "data": {
                "items": [{"chat_id": "oc_g", "chat_mode": "group"}],
                "page_token": "tok_2",
                "has_more": true
            }
        });
        assert_eq!(parse_chat_list_next_page(&paged).as_deref(), Some("tok_2"));
    }

    #[test]
    fn merge_catchup_keeps_stored_p2p_when_list_is_groups_or_empty() {
        let now = 1_700_000_000_000u64;
        let mut stored = HashMap::new();
        stored.insert("oc_dm".into(), catchup_floor_cursor(now, false));
        let listed = vec![("oc_g".into(), true)];
        let merged = merge_catchup_targets(stored.clone(), &listed, now);
        assert!(merged.contains_key("oc_dm"));
        assert!(merged.contains_key("oc_g"));
        assert!(!merged["oc_dm"].is_group);
        assert!(merged["oc_g"].is_group);
        let after_400 = merge_catchup_targets(stored, &[], now);
        assert_eq!(after_400.len(), 1);
        assert!(after_400.contains_key("oc_dm"));
    }

    #[test]
    fn p2p_entered_event_yields_chat_id() {
        let v = json!({
            "header": {"event_type": "im.chat.access_event.bot_p2p_chat_entered_v1"},
            "event": {"chat_id": "oc_new_dm", "operator_id": {"open_id": "ou_x"}}
        });
        assert_eq!(p2p_entered_chat_id(&v).as_deref(), Some("oc_new_dm"));
        assert!(
            p2p_entered_chat_id(&json!({"header": {"event_type": "im.message.receive_v1"}}))
                .is_none()
        );
    }

    #[test]
    fn list_item_user_text_becomes_native() {
        let ep = ep_with(&[]);
        let item = json!({
            "message_id": "om_c",
            "chat_id": "oc_dm",
            "msg_type": "text",
            "create_time": "1700000000000",
            "sender": {"id": "ou_user", "sender_type": "user"},
            "body": {"content": "{\"text\":\"补上刚才那条\"}"}
        });
        let event = list_item_to_event(&item, false).unwrap();
        let env = native_from_event(&ep, &event).unwrap();
        assert_eq!(env.text, "补上刚才那条");
        assert_eq!(env.sender_id, "ou_user");
        assert_eq!(env.meta["message_id"], json!("om_c"));
        assert_eq!(env.meta["chat_id"], json!("oc_dm"));
        assert_eq!(env.meta["is_group"], json!(false));
        assert!(list_item_to_event(
            &json!({
                "message_id": "om_bot",
                "chat_id": "oc_dm",
                "sender": {"id": "cli_bot", "sender_type": "app"},
                "body": {"content": "{\"text\":\"ack\"}"}
            }),
            false
        )
        .is_none());
    }

    #[test]
    fn native_p2p_text_event() {
        let ep = ep_with(&[]);
        let v = json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {
                    "sender_id": {"open_id": "ou_user"},
                    "sender_type": "user"
                },
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_dm",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"hi bot\"}"
                }
            }
        });
        let env = native_from_envelope(&ep, &v).unwrap();
        assert_eq!(env.channel, "feishu");
        assert_eq!(env.sender_id, "ou_user");
        assert_eq!(env.text, "hi bot");
        assert_eq!(env.meta["chat_id"], json!("oc_dm"));
        assert_eq!(env.meta["is_group"], json!(false));
        assert_eq!(env.meta["message_id"], json!("om_1"));
        assert_eq!(env.meta["receive_id_type"], json!("open_id"));
        assert_eq!(env.meta["receive_id"], json!("ou_user"));
        assert_eq!(receive_id_type(&env), "open_id");
        assert_eq!(receive_id(&env, "open_id"), "ou_user");
    }

    #[test]
    fn native_group_uses_chat_id() {
        let ep = ep_with(&[]);
        let v = json!({
            "event": {
                "header": {"event_type": "im.message.receive_v1"},
                "sender": {"sender_id": {"open_id": "ou_2"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_g",
                    "chat_id": "oc_group",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": {"text": "hey"},
                    "mentions": [{"id": {"open_id": "ou_bot"}}]
                }
            }
        });
        let env = native_from_envelope(&ep, &v).unwrap();
        assert_eq!(env.meta["is_group"], json!(true));
        assert_eq!(env.meta["receive_id_type"], json!("chat_id"));
        assert_eq!(env.meta["receive_id"], json!("oc_group"));
        assert_eq!(env.meta["is_mentioned"], json!(true));
        assert_eq!(receive_id(&env, "chat_id"), "oc_group");
    }

    #[test]
    fn feishu_file_type_ext() {
        assert_eq!(feishu_file_type("a.pdf"), "pdf");
        assert_eq!(feishu_file_type("a.PDF"), "pdf");
        assert_eq!(feishu_file_type("notes.docx"), "doc");
        assert_eq!(feishu_file_type("clip.mp4"), "mp4");
        assert_eq!(feishu_file_type("bin.xyz"), "stream");
    }

    #[test]
    fn native_ingests_image() {
        let ep = ep_with(&[]);
        let v = json!({
            "event": {
                "sender": {"sender_id": {"open_id": "ou_u"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_i",
                    "chat_id": "oc_dm",
                    "chat_type": "p2p",
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_x\"}"
                }
            }
        });
        let env = native_from_envelope(&ep, &v).expect("image");
        assert_eq!(env.text, "[图片]");
        assert_eq!(env.meta["image_key"], json!("img_x"));
        assert_eq!(env.meta["resource_key"], json!("img_x"));
        assert_eq!(env.meta["resource_type"], json!("image"));
    }

    #[test]
    fn skips_bot_sender() {
        let ep = ep_with(&[]);
        let v = json!({
            "event": {
                "sender": {"sender_id": {"open_id": "ou_bot"}, "sender_type": "app"},
                "message": {
                    "chat_id": "oc_x",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"nope\"}"
                }
            }
        });
        assert!(native_from_envelope(&ep, &v).is_none());
    }

    #[test]
    fn native_ingests_card_action_choice() {
        let ep = ep_with(&[]);
        let v = json!({
            "header": {"event_type": "card.action.trigger", "event_id": "ev_card_1"},
            "event": {
                "operator": {"open_id": "ou_user", "name": "William"},
                "action": {"tag": "button", "value": {"choice": "2"}},
                "context": {
                    "open_chat_id": "oc_dm",
                    "open_message_id": "om_card",
                    "chat_type": "p2p"
                }
            }
        });
        let env = native_from_envelope(&ep, &v).expect("card action");
        assert_eq!(env.sender_id, "ou_user");
        assert_eq!(env.query_text(), "2");
        assert!(env.is_choice_click());
        assert_eq!(env.meta["is_mentioned"], json!(true));
        assert_eq!(env.meta["is_group"], json!(false));
        assert_eq!(env.meta["message_id"], json!("ev_card_1"));
        let group = json!({
            "header": {"event_type": "card.action.trigger", "event_id": "ev_g"},
            "event": {
                "operator": {"open_id": "ou_user"},
                "action": {"value": {"choice": "api"}},
                "host": "im_group_chat",
                "context": {"open_chat_id": "oc_group", "chat_type": "group"}
            }
        });
        let g = native_from_envelope(&ep, &group).unwrap();
        assert_eq!(g.query_text(), "api");
        assert_eq!(g.meta["is_group"], json!(true));
        assert_eq!(g.meta["receive_id"], json!("oc_group"));
        let other_card = json!({
            "header": {"event_type": "im.message.card_update"},
            "event": {
                "operator": {"open_id": "ou_user"},
                "action": {"value": {"choice": "1"}}
            }
        });
        assert!(native_from_envelope(&ep, &other_card).is_none());
    }

    #[test]
    fn choice_card_puts_choice_on_buttons() {
        let card = feishu_choice_card(
            "pick",
            &[
                ("1".into(), "Allow".into()),
                ("2".into(), "Always".into()),
                ("3".into(), "Deny".into()),
            ],
        );
        assert_eq!(card["elements"][1]["actions"][0]["value"]["choice"], "1");
        assert_eq!(card["elements"][1]["actions"][0]["type"], "primary");
        assert_eq!(card["elements"][1]["actions"][2]["type"], "danger");
    }

    #[test]
    fn protobuf_ping_roundtrip() {
        let f = PbFrame::ping(42);
        let bytes = f.encode();
        let d = PbFrame::decode(&bytes).unwrap();
        assert_eq!(d.service, 42);
        assert_eq!(d.method, PB_CONTROL);
        assert_eq!(d.header("type"), "ping");
        assert_eq!(d.seq_id, 0);
    }

    #[test]
    fn protobuf_event_payload_is_json() {
        let payload = serde_json::to_vec(&json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_z"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_z",
                    "chat_id": "oc_z",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"from pb\"}"
                }
            }
        }))
        .unwrap();
        let frame = PbFrame {
            method: PB_DATA,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "event".into(),
            }],
            payload,
            ..PbFrame::default()
        };
        let d = PbFrame::decode(&frame.encode()).unwrap();
        assert_eq!(d.method, PB_DATA);
        let v: Value = serde_json::from_slice(&d.payload).unwrap();
        let ep = ep_with(&[]);
        let env = native_from_envelope(&ep, &v).unwrap();
        assert_eq!(env.text, "from pb");
        assert_eq!(env.sender_id, "ou_z");
    }

    #[test]
    fn json_ack_when_need_ack() {
        let v = json!({"type": "event", "headers": {"need_ack": true, "message_id": "m1"}});
        let ack = json_ack(&v).unwrap();
        assert!(ack.contains("m1"));
        assert!(ack.contains("ack"));
    }

    #[test]
    fn typing_ok_needs_message_id() {
        let mut env = NativePayload::default();
        env.channel = "feishu".into();
        assert!(!typing_ok(&env));
        env.meta.insert("message_id".into(), json!("om_1"));
        assert!(typing_ok(&env));
        assert_eq!(typing_message_id(&env), "om_1");
    }

    #[test]
    fn first_send_replies_to_inbound_message() {
        let mut env = NativePayload::default();
        env.channel = "feishu".into();
        let open = send_create_url("https://open.feishu.cn", &env, "open_id");
        assert!(open.contains("receive_id_type=open_id"), "{open}");
        assert!(!send_is_reply(&open));
        env.meta.insert("message_id".into(), json!("om/1"));
        let reply = send_create_url("https://open.feishu.cn", &env, "open_id");
        assert!(reply.ends_with("/im/v1/messages/om%2F1/reply"), "{reply}");
        assert!(send_is_reply(&reply));
    }

    #[test]
    fn typing_reaction_urls_and_body() {
        let body = typing_create_body();
        assert_eq!(body["reaction_type"]["emoji_type"], json!(TYPING_EMOJI));
        let create = typing_create_url("https://open.feishu.cn", "om/1");
        assert!(create.ends_with("/im/v1/messages/om%2F1/reactions"));
        let del = typing_delete_url("https://open.feishu.cn", "om_1", "re/a");
        assert!(del.ends_with("/im/v1/messages/om_1/reactions/re%2Fa"));
        assert_eq!(
            parse_reaction_id(&json!({"code": 0, "data": {"reaction_id": "re_9"}})).as_deref(),
            Some("re_9")
        );
        assert!(parse_reaction_id(&json!({"code": 0, "data": {}})).is_none());
        assert!(feishu_api_ok(&json!({"code": 0})));
        assert!(!feishu_api_ok(&json!({"code": 99992351})));
    }

    #[test]
    fn parse_sent_message_id_from_create() {
        assert_eq!(
            parse_sent_message_id(&json!({"code": 0, "data": {"message_id": "om_p"}})).as_deref(),
            Some("om_p")
        );
        assert!(parse_sent_message_id(&json!({"code": 0, "data": {}})).is_none());
        assert_eq!(
            sent_chat_meta(&json!({
                "code": 0,
                "data": {
                    "message_id": "om_sent",
                    "chat_id": "oc_from_send",
                    "create_time": "1700000000000"
                }
            })),
            Some((
                "oc_from_send".into(),
                "om_sent".into(),
                "1700000000000".into()
            ))
        );
        assert!(sent_chat_meta(&json!({"code": 0, "data": {"message_id": "om_p"}})).is_none());
    }

    #[test]
    fn pick_ws_url_reads_sdk_fields() {
        assert_eq!(
            pick_ws_url(&json!({"code": 0, "data": {"URL": "wss://msg-frontier.feishu.cn/ws/v2"}}))
                .as_deref(),
            Some("wss://msg-frontier.feishu.cn/ws/v2")
        );
        assert_eq!(
            pick_ws_url(&json!({"data": {"url": "wss://example/ws"}})).as_deref(),
            Some("wss://example/ws")
        );
    }

    #[test]
    fn patch_url_and_bubble_slot() {
        let url = patch_url("https://open.feishu.cn", "om/1");
        assert!(url.ends_with("/im/v1/messages/om%2F1"));
        store_bubble("c:1", "om_p".into());
        assert_eq!(peek_slot("c:1").map(|s| s.mid).as_deref(), Some("om_p"));
        bump_bubble("c:1");
        assert_eq!(peek_slot("c:1").map(|s| s.patches), Some(1));
        assert_eq!(take_bubble("c:1").as_deref(), Some("om_p"));
        assert!(peek_slot("c:1").is_none());
    }

    #[test]
    fn list_messages_uses_chat_container_type() {
        let url = list_messages_url("https://open.feishu.cn/", "oc_16af");
        assert!(url.contains("container_id_type=chat&"), "{url}");
        assert!(!url.contains("container_id_type=chat_id"), "{url}");
        assert!(url.contains("container_id=oc_16af"), "{url}");
    }

    #[test]
    fn list_chats_url_has_sort_and_user_id_type() {
        let url = list_chats_url("https://open.feishu.cn/", None);
        assert!(
            url.starts_with("https://open.feishu.cn/open-apis/im/v1/chats?"),
            "{url}"
        );
        assert!(!url.contains("open.feishu.cn//"), "{url}");
        assert!(url.contains("user_id_type=open_id"), "{url}");
        assert!(url.contains("sort_type=ByCreateTimeAsc"), "{url}");
        assert!(url.contains("page_size=100"), "{url}");
        let next = list_chats_url("https://open.feishu.cn", Some("tok/1"));
        assert!(next.contains("page_token=tok%2F1"), "{next}");
    }

    #[test]
    fn feishu_edit_cap_rotates_before_20() {
        assert!(!should_rotate_feishu_bubble(0));
        assert!(!should_rotate_feishu_bubble(17));
        assert!(should_rotate_feishu_bubble(18));
        assert!(should_rotate_feishu_bubble(20));
        assert!(is_feishu_edit_exhausted(&Error::msg(
            "feishu patch HTTP 400 Bad Request code=230072 msg=The message has reached the number of times it can be edited."
        )));
        assert!(!is_feishu_edit_exhausted(&Error::msg(
            "feishu patch HTTP 400 Bad Request code=230002 msg=invalid"
        )));
        assert!(is_feishu_illegal_content(&Error::msg(
            "feishu send HTTP 400 Bad Request code=230001 msg=content is illegal"
        )));
        assert!(!is_feishu_illegal_content(&Error::msg(
            "feishu patch HTTP 400 Bad Request code=230072 msg=edited"
        )));
    }
}
