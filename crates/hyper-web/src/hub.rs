use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use hyper_loop::clarify::{ClarifyDecision, ClarifyHub, ClarifyRequest};
use hyper_loop::config::Config;
use hyper_loop::media::MediaPart;
use hyper_loop::permit::{PermitDecision, PermitHub, PermitRequest};
use hyper_loop::session::{DeltaChannel, SessionEvent, StoredMedia};
use hyper_loop::sidecar::{
    execute_turn, Dispatch, EventSink, RpcRequest, SidecarSession, TurnRequest, TurnResult,
};
use hyper_loop::CancelFlag;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::cron::{heartbeat_prompt, now_s, CronStore};
use crate::oauth_flow::OauthFlow;
use crate::office::OfficeSaves;
use crate::office_runtime::{office_auto_enabled, EnsureOpts, OfficeBoot};

pub type Bus = broadcast::Sender<Value>;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Mutex<Inner>>,
    pub bus: Bus,
    pub oauth: Arc<OauthFlow>,
    pub office_saves: Arc<OfficeSaves>,
    pub office_boot: OfficeBoot,
}

pub struct Inner {
    pub session: SidecarSession,
    /// Parked console sessions that still have a live turn (or were switched
    /// away from). Keyed by session id. Focused session is `session`, not here.
    pub parked: HashMap<String, SidecarSession>,
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub live: HashMap<String, LiveTurn>,
    pub permit: PermitHub,
    pub pending: VecDeque<PendingPermit>,
    pub permit_seq: u64,
    pub clarify: ClarifyHub,
    pub pending_clarify: VecDeque<PendingClarify>,
    pub clarify_seq: u64,
    pub agents_md: bool,
    pub agents_md_head: bool,
    pub cron: CronStore,
    /// endpoint id → 运行状态,watcher 重建、serve 任务退出时更新
    pub channel_runtime: HashMap<String, ChannelRuntime>,
    /// watcher 代际,防止被 abort 的旧 serve 任务写入过期状态
    channel_gen: u64,
    ev_tx: mpsc::UnboundedSender<(String, SessionEvent)>,
    bus: Bus,
}

impl Inner {
    pub fn any_live(&self) -> bool {
        !self.live.is_empty()
    }

    fn session_mut(&mut self, id: &str) -> Option<&mut SidecarSession> {
        if self.session.session_id() == id {
            Some(&mut self.session)
        } else {
            self.parked.get_mut(id)
        }
    }

    pub(crate) fn console_state(&self) -> serde_json::Value {
        let mut v = self.session.state_json();
        let mut running: Vec<String> = self.live.keys().cloned().collect();
        running.sort();
        v["running"] = json!(running);
        let mut started = serde_json::Map::new();
        for (id, live) in &self.live {
            started.insert(id.clone(), json!(live.started_ms));
        }
        v["running_started"] = json!(started);
        v
    }

    fn focused_permit(&self) -> Option<&PendingPermit> {
        modal_for_session(&self.pending, self.session.session_id(), |p| &p.req.session)
    }

    fn focused_clarify(&self) -> Option<&PendingClarify> {
        modal_for_session(&self.pending_clarify, self.session.session_id(), |p| {
            &p.req.session
        })
    }

    pub(crate) fn focused_permit_json(&self) -> Option<Value> {
        self.focused_permit().map(|p| p.json())
    }

    pub(crate) fn focused_clarify_json(&self) -> Option<Value> {
        self.focused_clarify().map(|p| p.json())
    }
}

/// 单个频道 endpoint 的运行状态,GET /api/channels 的 `runtime` 字段
#[derive(Clone)]
pub struct ChannelRuntime {
    pub state: &'static str, // running | error | no_credentials | off
    pub detail: Option<String>,
}

impl ChannelRuntime {
    pub fn json(&self) -> Value {
        json!({"state": self.state, "detail": self.detail})
    }
}

pub struct LiveTurn {
    pub cancel: CancelFlag,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
    pub started_ms: u64,
}

pub struct PendingPermit {
    pub id: u64,
    pub req: PermitRequest,
}

impl PendingPermit {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "tool": self.req.ask.tool,
            "preview": self.req.ask.preview,
            "session": self.req.session,
        })
    }
}

pub struct PendingClarify {
    pub id: u64,
    pub req: ClarifyRequest,
}

impl PendingClarify {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.req.ask.title,
            "prompt": self.req.ask.prompt,
            "options": self.req.ask.options.iter().map(|o| json!({
                "id": o.id,
                "label": o.label,
            })).collect::<Vec<_>>(),
            "session": self.req.session,
        })
    }
}

impl AppState {
    pub fn new(
        session: SidecarSession,
        cfg: Config,
        cfg_path: PathBuf,
        agents_md: bool,
        agents_md_head: bool,
    ) -> Result<Self> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<(String, SessionEvent)>();
        // Token deltas are tiny but frequent. 512 filled up around ~30 tool hops
        // on Windows (slow JSON/React), RecvError::Lagged then pushed the whole
        // history over WS and froze the UI. Keep a deeper ring; still never put
        // the transcript on this bus (see resync / history.replace refetch).
        let (bus, _) = broadcast::channel(8192);
        let bus_fwd = bus.clone();
        tokio::spawn(async move {
            forward_session_events(ev_rx, bus_fwd).await;
        });
        let approvals = session.approvals();
        let (permit, permit_rx) = PermitHub::pair(approvals);
        let (clarify, clarify_rx) = ClarifyHub::pair();
        let inner = Arc::new(Mutex::new(Inner {
            session,
            parked: HashMap::new(),
            cfg,
            cfg_path,
            live: HashMap::new(),
            permit,
            pending: VecDeque::new(),
            permit_seq: 0,
            clarify,
            pending_clarify: VecDeque::new(),
            clarify_seq: 0,
            agents_md,
            agents_md_head,
            cron: CronStore::load(),
            channel_runtime: HashMap::new(),
            channel_gen: 0,
            ev_tx,
            bus: bus.clone(),
        }));
        if let Ok(h) = Config::home_dir() {
            hyper_loop::subagent::reap_orphans(&h.join("sessions"));
        }
        let pending_state = inner.clone();
        let bus_p = bus.clone();
        tokio::spawn(async move {
            let mut rx = permit_rx;
            while let Some(req) = rx.recv().await {
                let mut g = pending_state.lock().await;
                g.permit_seq += 1;
                let id = g.permit_seq;
                let focused = g.session.session_id().to_string();
                let for_focus = session_matches(&req.session, &focused);
                let already = g
                    .pending
                    .iter()
                    .any(|p| session_matches(&p.req.session, &focused));
                let p = PendingPermit { id, req };
                let payload = p.json();
                g.pending.push_back(p);
                if for_focus && !already {
                    let _ = bus_p.send(notify("permit.ask", payload));
                }
            }
        });
        let pending_clarify = inner.clone();
        let bus_c = bus.clone();
        tokio::spawn(async move {
            let mut rx = clarify_rx;
            while let Some(req) = rx.recv().await {
                let mut g = pending_clarify.lock().await;
                g.clarify_seq += 1;
                let id = g.clarify_seq;
                let focused = g.session.session_id().to_string();
                let for_focus = session_matches(&req.session, &focused);
                let already = g
                    .pending_clarify
                    .iter()
                    .any(|p| session_matches(&p.req.session, &focused));
                let p = PendingClarify { id, req };
                let payload = p.json();
                g.pending_clarify.push_back(p);
                if for_focus && !already {
                    let _ = bus_c.send(notify("clarify.ask", payload));
                }
            }
        });
        Ok(Self {
            inner,
            bus,
            oauth: Arc::new(OauthFlow::new()),
            office_saves: Arc::new(OfficeSaves::default()),
            office_boot: OfficeBoot::default(),
        })
    }

    pub fn spawn_office_boot(&self) {
        if !office_auto_enabled() {
            return;
        }
        let boot = self.office_boot.clone();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            boot.set(true, "正在启动文档服务…");
            let office = {
                let g = inner.lock().await;
                g.cfg.office.clone()
            };
            match crate::office_runtime::ensure_office(&office, EnsureOpts::web_auto()).await {
                Ok(r) if r.ready => boot.set(false, ""),
                Ok(_) => boot.set(false, "文档服务仍在启动，稍后重新打开文件即可完整编辑。"),
                Err(e) => {
                    eprintln!("office: {e}");
                    boot.set(false, e.user_hint());
                }
            }
        });
    }

    pub fn spawn_background(&self) {
        spawn_channel_watch(self.inner.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let mut g = inner.lock().await;
                g.cron = CronStore::reload(g.session.workspace(), &g.cron);
                if g.session.turn_in_flight() || g.live.contains_key(g.session.session_id()) {
                    continue;
                }
                let now = now_s();
                let due: Vec<String> = g.cron.due(now);
                if let Some(id) = due.into_iter().next() {
                    if let Some(prompt) = g.cron.mark(&id, now) {
                        let _ = g.cron.save_with_workspace(g.session.workspace());
                        start_turn(
                            &mut g,
                            inner.clone(),
                            prompt,
                            Vec::new(),
                            Some(CronRetry::Job { id }),
                            None,
                        );
                        continue;
                    }
                }
                if g.cron.heartbeat_due(now) {
                    let prompt = heartbeat_prompt(&g.cron, g.session.workspace());
                    g.cron.heartbeat.last_run = Some(now);
                    let _ = g.cron.save();
                    start_turn(
                        &mut g,
                        inner.clone(),
                        prompt,
                        Vec::new(),
                        Some(CronRetry::Heartbeat),
                        None,
                    );
                }
            }
        });
    }

    pub async fn rpc(&self, method: &str, params: Option<Value>) -> Value {
        let mut g = self.inner.lock().await;
        if method == "slash" {
            let text = params
                .as_ref()
                .and_then(|p| p.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if text == "/reload" {
                if let Ok(disk) = Config::load_from(&g.cfg_path) {
                    g.cfg = disk;
                }
                g.session.refresh_surface();
            }
        }
        if method == "session.delete" {
            abort_deleted_sessions(&mut g, params.as_ref());
        }
        if should_park_switch(method, params.as_ref()) {
            match park_for_switch(&mut g, method, params.as_ref()) {
                Ok(ParkOutcome::Done(v)) => {
                    push_focused_modals(&g);
                    push_state(&g);
                    let _ = g.bus.send(notify(
                        "history.replace",
                        json!({
                            "events": console_events(g.session.events()),
                            "session": g.session.session_id(),
                        }),
                    ));
                    return v;
                }
                Ok(ParkOutcome::Continue) => {}
                Err(err) => return json!({"ok": false, "error": err}),
            }
        }
        let before = g.session.session_id().to_string();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        let dispatch = g.session.handle(&req);
        let out = apply_dispatch(&mut g, self.inner.clone(), dispatch);
        // 聊天 /approvals 只改 session+写盘;这里回读同步 PermitHub,闸门才真正生效
        g.permit.set_mode(g.session.approvals());
        if g.session.session_id() != before {
            push_focused_modals(&g);
            let _ = g.bus.send(notify(
                "history.replace",
                json!({
                    "events": console_events(g.session.events()),
                    "session": g.session.session_id(),
                }),
            ));
        }
        out
    }

    pub async fn decide_permit(&self, id: u64, decision: PermitDecision) -> Result<Value, String> {
        let mut g = self.inner.lock().await;
        let idx = g
            .pending
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| "no matching permit".to_string())?;
        let p = g.pending.remove(idx).expect("idx");
        if decision == PermitDecision::Always {
            g.permit.remember(&p.req.ask.tool);
        }
        let _ = p.req.reply.send(decision);
        push_focused_modals(&g);
        Ok(json!({"ok": true, "id": id}))
    }

    pub async fn decide_clarify(
        &self,
        id: u64,
        decision: ClarifyDecision,
    ) -> Result<Value, String> {
        let mut g = self.inner.lock().await;
        let idx = g
            .pending_clarify
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| "no matching clarify".to_string())?;
        let p = g.pending_clarify.remove(idx).expect("idx");
        let _ = p.req.reply.send(decision);
        push_focused_modals(&g);
        Ok(json!({"ok": true, "id": id}))
    }
}

fn extra_len(ep: &hyper_loop::ChannelEndpoint, keys: &[&str]) -> usize {
    keys.iter()
        .filter_map(|k| ep.extra.get(*k))
        .map(|s| s.trim().len())
        .find(|n| *n > 0)
        .unwrap_or(0)
}

fn extra_has(ep: &hyper_loop::ChannelEndpoint, keys: &[&str]) -> bool {
    extra_len(ep, keys) > 0
}

/// True when this process can start a live client for `ep`.
fn endpoint_runnable(ep: &hyper_loop::ChannelEndpoint) -> bool {
    if !ep.enabled {
        return false;
    }
    match ep.kind.to_ascii_lowercase().as_str() {
        "telegram" => extra_has(ep, &["bot_token", "token"]),
        "webhook" | "http" | "console" => true,
        "qq" => extra_has(ep, &["app_id"]) && extra_has(ep, &["client_secret"]),
        "wechat" => extra_has(ep, &["bot_token", "token"]),
        "wecom" => extra_has(ep, &["bot_id"]) && extra_has(ep, &["secret"]),
        "dingtalk" => extra_has(ep, &["client_id"]) && extra_has(ep, &["client_secret"]),
        "feishu" => {
            extra_has(ep, &["app_id", "client_id"])
                && extra_has(ep, &["app_secret", "client_secret"])
        }
        _ => false,
    }
}

/// 该 kind 的适配器是否在本进程内(与 [`endpoint_runnable`] 的分支一致)。
fn endpoint_in_process(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "telegram"
            | "webhook"
            | "http"
            | "console"
            | "qq"
            | "wechat"
            | "wecom"
            | "dingtalk"
            | "feishu"
    )
}

/// 不含运行结果的静态分类:off / no_credentials / running(即将启动)。
pub fn endpoint_static_runtime(ep: &hyper_loop::ChannelEndpoint) -> ChannelRuntime {
    let state = if !ep.enabled || !endpoint_in_process(&ep.kind) {
        "off"
    } else if !endpoint_runnable(ep) {
        "no_credentials"
    } else {
        "running"
    };
    ChannelRuntime {
        state,
        detail: None,
    }
}

/// 字符安全截断(错误文本进 runtime.detail 前压到 300 字符)。
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// 每个 endpoint 整体序列化后拼指纹:allow/deny、策略、bind、凭据内容任何
/// 变化都会触发 watcher 重启(旧版只看 id/kind/enabled/凭据长度)。
fn channels_fingerprint(cfg: &Config) -> String {
    let mut rows: Vec<String> = cfg
        .channels
        .endpoints
        .iter()
        .map(|e| serde_json::to_string(e).unwrap_or_default())
        .collect();
    rows.sort();
    rows.join("|")
}

fn spawn_channel_watch(inner: Arc<Mutex<Inner>>) {
    tokio::spawn(async move {
        let mut last = String::new();
        let mut jobs: Vec<JoinHandle<()>> = Vec::new();
        loop {
            let (sig, cfg, workspace) = {
                let g = inner.lock().await;
                (
                    channels_fingerprint(&g.cfg),
                    g.cfg.clone(),
                    g.session.workspace().to_path_buf(),
                )
            };
            if sig != last {
                last = sig.clone();
                for j in jobs.drain(..) {
                    j.abort();
                }
                // 重建全量运行状态表并换代,旧任务的退出回写会被代际拦下
                let gen = {
                    let mut g = inner.lock().await;
                    g.channel_gen += 1;
                    g.channel_runtime = cfg
                        .channels
                        .endpoints
                        .iter()
                        .map(|e| (e.id.clone(), endpoint_static_runtime(e)))
                        .collect();
                    g.channel_gen
                };
                for ep in cfg
                    .channels
                    .endpoints
                    .iter()
                    .filter(|e| endpoint_runnable(e))
                {
                    let ep = ep.clone();
                    let cfg = cfg.clone();
                    let workspace = workspace.clone();
                    let inner_ep = inner.clone();
                    eprintln!("hyper {}: starting in-process client ({})", ep.kind, ep.id);
                    jobs.push(tokio::spawn(async move {
                        let id = ep.id.clone();
                        let kind = ep.kind.clone();
                        // Adapter exit / panic / lock fight: backoff and start again.
                        // Fingerprint change aborts this task; gen guards stale writes.
                        hyper_loop::channel::keep_client_watched(
                            &kind,
                            &id,
                            {
                                let cfg = cfg.clone();
                                let workspace = workspace.clone();
                                let ep = ep.clone();
                                move || {
                                    hyper_loop::channel::serve_endpoint(
                                        cfg.clone(),
                                        workspace.clone(),
                                        ep.clone(),
                                    )
                                }
                            },
                            {
                                let inner_ep = inner_ep.clone();
                                let id = id.clone();
                                move |st| {
                                    let inner_ep = inner_ep.clone();
                                    let id = id.clone();
                                    async move {
                                        let (state, detail) = match st {
                                            hyper_loop::channel::ClientWatch::Running => {
                                                ("running", None)
                                            }
                                            hyper_loop::channel::ClientWatch::Retry {
                                                detail,
                                                wait_secs,
                                            } => (
                                                "error",
                                                Some(clip_chars(
                                                    &format!("retry in {wait_secs}s: {detail}"),
                                                    300,
                                                )),
                                            ),
                                            hyper_loop::channel::ClientWatch::Fatal { detail } => {
                                                ("error", Some(clip_chars(&detail, 300)))
                                            }
                                        };
                                        let mut g = inner_ep.lock().await;
                                        if g.channel_gen == gen {
                                            g.channel_runtime
                                                .insert(id, ChannelRuntime { state, detail });
                                        }
                                    }
                                }
                            },
                        )
                        .await;
                    }));
                }
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    });
}

pub fn notify(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn should_park_switch(method: &str, params: Option<&Value>) -> bool {
    matches!(method, "session.resume" | "session.new") || is_switch_slash(method, params)
}

fn is_switch_slash(method: &str, params: Option<&Value>) -> bool {
    if method != "slash" {
        return false;
    }
    let t = params
        .and_then(|p| p.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    t == "/new" || t.starts_with("/new ") || t == "/clear"
}

enum ParkOutcome {
    Continue,
    Done(Value),
}

fn park_for_switch(
    inner: &mut Inner,
    method: &str,
    params: Option<&Value>,
) -> Result<ParkOutcome, String> {
    if method == "session.resume" {
        if let Some(q) = param_session_query(params) {
            if q == inner.session.session_id() {
                return Ok(ParkOutcome::Continue);
            }
            if inner.parked.contains_key(&q) {
                focus_parked(inner, &q);
                return Ok(ParkOutcome::Done(json!({
                    "ok": true,
                    "session": inner.session.session_id(),
                    "title": inner.session.title(),
                })));
            }
        }
        focus_twin(inner);
        return Ok(ParkOutcome::Continue);
    }
    if method == "session.new" || is_switch_slash(method, params) {
        focus_twin(inner);
        return Ok(ParkOutcome::Continue);
    }
    Ok(ParkOutcome::Continue)
}

fn param_session_query(params: Option<&Value>) -> Option<String> {
    let p = params?;
    for key in ["session", "search", "text", "prompt"] {
        if let Some(s) = p
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_string());
        }
    }
    None
}

fn focus_twin(inner: &mut Inner) {
    let twin = inner.session.idle_twin();
    park_current(inner, twin);
}

fn focus_parked(inner: &mut Inner, id: &str) {
    let Some(mut next) = inner.parked.remove(id) else {
        return;
    };
    next.reload();
    park_current(inner, next);
}

fn park_current(inner: &mut Inner, next: SidecarSession) {
    let old = std::mem::replace(&mut inner.session, next);
    let id = old.session_id().to_string();
    if !id.is_empty() && id != inner.session.session_id() {
        inner.parked.insert(id, old);
    }
}

fn abort_deleted_sessions(inner: &mut Inner, params: Option<&Value>) {
    let focused = inner.session.session_id().to_string();
    for id in delete_ids(params) {
        if id == focused {
            continue;
        }
        if let Some(live) = inner.live.get(&id) {
            live.cancel.cancel();
        }
        inner.parked.remove(&id);
        drop_session_modals(inner, &id);
    }
}

fn delete_ids(params: Option<&Value>) -> Vec<String> {
    let Some(p) = params else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    if let Some(s) = p
        .get("session")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        ids.push(s.to_string());
    }
    if let Some(arr) = p.get("sessions").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                ids.push(s.to_string());
            }
        }
    }
    ids
}

/// Session events for the console: drop inline `data:` URLs so hello / history
/// cannot replay ComputerUse screenshots as multi-megabyte JSON.
pub fn console_events(events: &[SessionEvent]) -> Value {
    let mut v = serde_json::to_value(events).unwrap_or_else(|_| json!([]));
    redact_data_uris(&mut v);
    v
}

fn redact_data_uris(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(url)) = map.get_mut("url") {
                if url.starts_with("data:") {
                    url.clear();
                }
            }
            for child in map.values_mut() {
                redact_data_uris(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                redact_data_uris(child);
            }
        }
        _ => {}
    }
}

const DELTA_FLUSH_MS: u64 = 32;
const DELTA_FLUSH_CHARS: usize = 4096;

struct DeltaBuf {
    reason: String,
    content: String,
}

impl DeltaBuf {
    fn pending(&self) -> bool {
        !self.reason.is_empty() || !self.content.is_empty()
    }
}

/// Merge consecutive token deltas so one WS frame covers ~32ms of tokens.
async fn forward_session_events(
    mut ev_rx: mpsc::UnboundedReceiver<(String, SessionEvent)>,
    bus: Bus,
) {
    let mut bufs: HashMap<String, DeltaBuf> = HashMap::new();
    loop {
        let pending = bufs.values().any(DeltaBuf::pending);
        let next = if pending {
            tokio::select! {
                ev = ev_rx.recv() => ev,
                _ = tokio::time::sleep(Duration::from_millis(DELTA_FLUSH_MS)) => {
                    flush_all_deltas(&mut bufs, &bus);
                    continue;
                }
            }
        } else {
            ev_rx.recv().await
        };
        let Some((sid, ev)) = next else {
            flush_all_deltas(&mut bufs, &bus);
            break;
        };
        match ev {
            SessionEvent::Delta(d) if d.reset => {
                flush_session_deltas(&sid, &mut bufs, &bus);
                let _ = bus.send(notify(
                    "event.append",
                    event_payload(&sid, SessionEvent::Delta(d)),
                ));
            }
            SessionEvent::Delta(d) => {
                let buf = bufs.entry(sid.clone()).or_insert_with(|| DeltaBuf {
                    reason: String::new(),
                    content: String::new(),
                });
                match d.channel {
                    DeltaChannel::Reasoning => buf.reason.push_str(&d.text),
                    DeltaChannel::Content => buf.content.push_str(&d.text),
                }
                if buf.reason.len() + buf.content.len() >= DELTA_FLUSH_CHARS {
                    flush_session_deltas(&sid, &mut bufs, &bus);
                }
            }
            other => {
                flush_session_deltas(&sid, &mut bufs, &bus);
                let _ = bus.send(notify("event.append", event_payload(&sid, other)));
            }
        }
    }
}

fn flush_all_deltas(bufs: &mut HashMap<String, DeltaBuf>, bus: &Bus) {
    let ids: Vec<String> = bufs.keys().cloned().collect();
    for id in ids {
        flush_session_deltas(&id, bufs, bus);
    }
}

fn flush_session_deltas(sid: &str, bufs: &mut HashMap<String, DeltaBuf>, bus: &Bus) {
    let Some(buf) = bufs.get_mut(sid) else {
        return;
    };
    if !buf.reason.is_empty() {
        let text = std::mem::take(&mut buf.reason);
        let _ = bus.send(notify(
            "event.append",
            event_payload(
                sid,
                SessionEvent::delta_chunk(DeltaChannel::Reasoning, text),
            ),
        ));
    }
    if !buf.content.is_empty() {
        let text = std::mem::take(&mut buf.content);
        let _ = bus.send(notify(
            "event.append",
            event_payload(sid, SessionEvent::delta_chunk(DeltaChannel::Content, text)),
        ));
    }
}

fn event_payload(session: &str, ev: SessionEvent) -> Value {
    let mut v = serde_json::to_value(&ev).unwrap_or(json!(null));
    if !session.is_empty() {
        if let Some(o) = v.as_object_mut() {
            o.insert("session".into(), json!(session));
        }
    }
    v
}

pub fn apply_dispatch(inner: &mut Inner, shared: Arc<Mutex<Inner>>, dispatch: Dispatch) -> Value {
    match dispatch {
        Dispatch::Result { result, events } => {
            let sid = inner.session.session_id().to_string();
            for e in &events {
                let _ = inner
                    .bus
                    .send(notify("event.append", event_payload(&sid, e.clone())));
            }
            push_state(inner);
            result
        }
        Dispatch::Error(err) => json!({"ok": false, "error": err.message, "code": err.code}),
        Dispatch::TurnStart { prompt, parts } => {
            start_turn(inner, shared, prompt, parts, None, None);
            json!({"ok": true, "started": true})
        }
        Dispatch::Abort => {
            abort_focused(inner);
            let sid = inner.session.session_id().to_string();
            drop_session_modals(inner, &sid);
            push_focused_modals(inner);
            push_state(inner);
            json!({"ok": true, "aborted": true})
        }
        Dispatch::AbortClear { cleared } => {
            abort_focused(inner);
            let sid = inner.session.session_id().to_string();
            drop_session_modals(inner, &sid);
            push_focused_modals(inner);
            push_state(inner);
            json!({"ok": true, "aborted": true, "cleared": cleared})
        }
    }
}

fn abort_focused(inner: &mut Inner) {
    let id = inner.session.session_id().to_string();
    if let Some(live) = inner.live.get(&id) {
        live.cancel.cancel();
    }
}

fn session_matches(tagged: &str, focused: &str) -> bool {
    tagged.is_empty() || tagged == focused
}

fn modal_for_session<'a, T>(
    items: impl IntoIterator<Item = &'a T>,
    focused: &str,
    session_of: impl Fn(&T) -> &str,
) -> Option<&'a T> {
    items
        .into_iter()
        .find(|p| session_matches(session_of(p), focused))
}

fn drop_session_modals(inner: &mut Inner, session_id: &str) {
    inner.pending.retain(|p| p.req.session != session_id);
    inner
        .pending_clarify
        .retain(|p| p.req.session != session_id);
}

fn push_focused_modals(inner: &Inner) {
    match inner.focused_permit() {
        Some(p) => {
            let _ = inner.bus.send(notify("permit.ask", p.json()));
        }
        None => {
            let _ = inner.bus.send(notify("permit.clear", json!(null)));
        }
    }
    match inner.focused_clarify() {
        Some(p) => {
            let _ = inner.bus.send(notify("clarify.ask", p.json()));
        }
        None => {
            let _ = inner.bus.send(notify("clarify.clear", json!(null)));
        }
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn push_state(inner: &Inner) {
    let _ = inner.bus.send(notify("state", inner.console_state()));
}

/// Retry bookkeeping for a cron-triggered turn. On error we defer the next
/// fire by `CRON_RETRY_DELAY_S` so a down LLM cannot write a stop-storm
/// every 1s tick, without sitting out the full `interval_s`.
pub(crate) enum CronRetry {
    Job { id: String },
    Heartbeat,
}

/// turn 任务 panic 兜底:正常路径 disarm;panic 时 Drop 收尾 turn、清 live 与
/// 待审批,否则 `live` 永远是 Some,整个控制台永久 busy 到重启。
struct TurnPanicGuard {
    shared: Arc<Mutex<Inner>>,
    session_id: String,
    armed: bool,
}

impl Drop for TurnPanicGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let sid = self.session_id.clone();
        if let Ok(mut g) = self.shared.try_lock() {
            cleanup_after_panic(&mut g, &sid);
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let shared = self.shared.clone();
            handle.spawn(async move {
                let mut g = shared.lock().await;
                cleanup_after_panic(&mut g, &sid);
            });
        }
    }
}

fn cleanup_after_panic(g: &mut Inner, session_id: &str) {
    let extra = if let Some(sess) = g.session_mut(session_id) {
        sess.finish_turn(&TurnResult::fail("internal error: turn task panicked"))
    } else {
        Vec::new()
    };
    for e in extra {
        let _ = g
            .bus
            .send(notify("event.append", event_payload(session_id, e)));
    }
    g.live.remove(session_id);
    drop_session_modals(g, session_id);
    push_focused_modals(g);
    push_state(g);
}

pub fn start_turn(
    inner: &mut Inner,
    shared: Arc<Mutex<Inner>>,
    prompt: String,
    parts: Vec<MediaPart>,
    cron_retry: Option<CronRetry>,
    session_id: Option<String>,
) {
    let sid = session_id.unwrap_or_else(|| inner.session.session_id().to_string());
    let (snapshot, steer) = {
        let Some(sess) = inner.session_mut(&sid) else {
            return;
        };
        sess.maybe_autotitle(&prompt);
        sess.begin_turn();
        (sess.snapshot(), sess.steer_slot())
    };
    // Agent 不把 User 事件转发给 live sink；这里带上 media，发送当下就能出缩略图。
    let stored: Vec<StoredMedia> = parts
        .iter()
        .map(|p| StoredMedia {
            kind: p.kind.as_str().into(),
            mime: p.mime.clone(),
            url: p.url.clone(),
        })
        .collect();
    let user = SessionEvent::user(&prompt).with_media(stored);
    let _ = inner
        .bus
        .send(notify("event.append", event_payload(&sid, user)));
    let cancel = CancelFlag::new();
    let (local_tx, mut local_rx) = mpsc::unbounded_channel();
    let tagged = inner.ev_tx.clone();
    let stamp = sid.clone();
    tokio::spawn(async move {
        while let Some(ev) = local_rx.recv().await {
            let _ = tagged.send((stamp.clone(), ev));
        }
    });
    let req = TurnRequest {
        prompt,
        parts,
        snapshot,
        cancel: cancel.clone(),
        emit: EventSink::new(local_tx),
        messages: Vec::new(),
        steer,
        persist: true,
        permit: Some(inner.permit.clone().with_session(&sid)),
        clarify: Some(inner.clarify.clone().with_session(&sid)),
    };
    let cfg = inner.cfg.clone();
    let agents_md = inner.agents_md;
    let agents_md_head = inner.agents_md_head;
    let turn_sid = sid.clone();
    let join = tokio::spawn(async move {
        let mut guard = TurnPanicGuard {
            shared: shared.clone(),
            session_id: turn_sid.clone(),
            armed: true,
        };
        let result = execute_turn(cfg, agents_md, agents_md_head, req).await;
        let mut g = shared.lock().await;
        guard.armed = false;
        let extra = if let Some(sess) = g.session_mut(&turn_sid) {
            sess.finish_turn(&result)
        } else {
            Vec::new()
        };
        let extra_has_stop = extra.iter().any(|e| matches!(e, SessionEvent::Stop(_)));
        for e in extra {
            let _ = g
                .bus
                .send(notify("event.append", event_payload(&turn_sid, e)));
        }
        g.live.remove(&turn_sid);
        if let Some(err) = result.error {
            match cron_retry {
                Some(CronRetry::Job { id }) => {
                    g.cron
                        .defer_job(&id, now_s(), crate::cron::CRON_RETRY_DELAY_S);
                    let _ = g.cron.save_with_workspace(g.session.workspace());
                }
                Some(CronRetry::Heartbeat) => {
                    g.cron
                        .defer_heartbeat(now_s(), crate::cron::CRON_RETRY_DELAY_S);
                    let _ = g.cron.save();
                }
                None => {}
            }
            if !extra_has_stop {
                let _ = g.bus.send(notify(
                    "event.append",
                    event_payload(&turn_sid, SessionEvent::stop(err)),
                ));
            }
        }
        // Streamed events already went out as event.append. Do not put the
        // whole JSONL on the bus — that payload alone lagged Windows clients
        // and triggered a hello/history death spiral. Catch-up is GET /history.
        let _ = g.bus.send(notify(
            "history.replace",
            json!({"refetch": true, "session": turn_sid}),
        ));
        let _ = g.bus.send(notify("state", g.console_state()));
        let follow = g.session_mut(&turn_sid).and_then(|s| s.pop_follow_up());
        if let Some((next, parts)) = follow {
            start_turn(&mut g, shared.clone(), next, parts, None, Some(turn_sid));
        }
    });
    inner.live.insert(
        sid,
        LiveTurn {
            cancel,
            join,
            started_ms: unix_ms(),
        },
    );
    push_state(inner);
}

pub fn redact_key(key: &str) -> String {
    let t = key.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.chars().count() <= 4 {
        return "****".into();
    }
    // Char-safe tail: byte slicing panics on non-ASCII keys.
    let tail: String = t
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

#[cfg(test)]
mod tests {
    use super::{
        channels_fingerprint, console_events, endpoint_static_runtime, event_payload, redact_key,
        session_matches,
    };
    use hyper_loop::config::Config;
    use hyper_loop::session::{SessionEvent, StoredMedia};
    use hyper_loop::ChannelEndpoint;

    #[test]
    fn redact_ascii_and_unicode() {
        assert_eq!(redact_key(""), "");
        assert_eq!(redact_key("ab"), "****");
        assert_eq!(redact_key("sk-abcdef1234"), "****1234");
        // Non-ASCII key: the old byte slice `&t[t.len()-4..]` panicked.
        assert_eq!(redact_key("密钥-abcdef"), "****cdef");
    }

    fn cfg_with(ep: ChannelEndpoint) -> Config {
        let mut cfg = Config::default();
        cfg.channels.endpoints = vec![ep];
        cfg
    }

    #[test]
    fn fingerprint_sees_policy_and_credential_changes() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        ep.extra.insert("bot_token".into(), "123:abc".into());
        let base = channels_fingerprint(&cfg_with(ep.clone()));

        let mut allow = ep.clone();
        allow.allow_from = vec!["alice".into()];
        assert_ne!(base, channels_fingerprint(&cfg_with(allow)), "allow_from");

        let mut token = ep.clone();
        token.extra.insert("bot_token".into(), "123:abd".into());
        assert_ne!(base, channels_fingerprint(&cfg_with(token)), "等长新凭据");

        let mut policy = ep.clone();
        policy.group_policy = "closed".into();
        assert_ne!(
            base,
            channels_fingerprint(&cfg_with(policy)),
            "group_policy"
        );
    }

    #[test]
    fn static_runtime_classification() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: false,
            ..ChannelEndpoint::default()
        };
        assert_eq!(endpoint_static_runtime(&ep).state, "off");
        ep.enabled = true;
        assert_eq!(endpoint_static_runtime(&ep).state, "no_credentials");
        ep.extra.insert("bot_token".into(), "123:abc".into());
        assert_eq!(endpoint_static_runtime(&ep).state, "running");
        ep.kind = "discord".into(); // 不在进程内的平台
        assert_eq!(endpoint_static_runtime(&ep).state, "off");
    }

    #[test]
    fn console_events_drop_inline_data_uris() {
        let ev = SessionEvent::tool("c1", "view", "Image loaded").with_media(vec![StoredMedia {
            kind: "image".into(),
            mime: "image/png".into(),
            url: "data:image/png;base64,AAAA".into(),
        }]);
        let path = SessionEvent::tool("c2", "view", "ok").with_media(vec![StoredMedia {
            kind: "image".into(),
            mime: "image/png".into(),
            url: ".grok-hyper/generated/shot.png".into(),
        }]);
        let v = console_events(&[ev, path]);
        assert_eq!(v[0]["media"][0]["url"], "");
        assert_eq!(v[1]["media"][0]["url"], ".grok-hyper/generated/shot.png");
    }

    #[test]
    fn event_payload_stamps_session() {
        let v = event_payload("abc", SessionEvent::user("hi"));
        assert_eq!(v["session"], "abc");
        assert_eq!(v["type"], "user");
        assert_eq!(v["text"], "hi");
    }

    #[test]
    fn modal_session_matches_focused_or_untagged() {
        assert!(session_matches("", "abc"));
        assert!(session_matches("abc", "abc"));
        assert!(!session_matches("abc", "def"));
    }
}
