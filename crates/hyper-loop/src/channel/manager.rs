//! Per-session queue + consume. Wash of QwenPaw `UnifiedQueueManager` +
//! `BaseChannel.consume_one` (debounce, then one worker per session).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

use crate::error::Result;
use crate::tool_calls::CancelFlag;

use super::access::{self, GateDecision};
use super::envelope::NativePayload;
use super::mailbox::{push_steer, take_steer, BusyPolicy, SteerSlot};
use super::progress::ImLocale;
use super::router::SessionRouter;
use super::ChannelsConfig;

const DEBOUNCE: Duration = Duration::from_millis(300);
const QUEUE_CAP: usize = 64;

/// Hermes per-adapter quiet period: mobile IM users type in bursts, and
/// 300ms only catches same-second pastes. WeChat iLink polls slowly, so its
/// window is widest; Feishu media gets a touch longer than text.
fn debounce_for(env: &NativePayload) -> Duration {
    let media = !env.media_parts().is_empty();
    match env.channel.to_ascii_lowercase().as_str() {
        "wechat" => Duration::from_millis(3000),
        "qq" => Duration::from_millis(1000),
        "feishu" if media => Duration::from_millis(800),
        "feishu" | "telegram" | "dingtalk" | "wecom" => Duration::from_millis(600),
        _ => DEBOUNCE,
    }
}

pub struct IngestResult {
    pub session_id: String,
    pub denied: Option<&'static str>,
}

#[derive(Clone)]
pub struct ChannelManager {
    inner: Arc<Mutex<Inner>>,
    ingest_tx: mpsc::Sender<NativePayload>,
}

struct Inner {
    cfg: ChannelsConfig,
    router: SessionRouter,
    sessions: HashMap<String, mpsc::Sender<NativePayload>>,
}

pub trait ChannelHandler: Send + Sync + 'static {
    fn handle(
        &self,
        env: NativePayload,
        cancel: CancelFlag,
        steer: SteerSlot,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<super::envelope::ContentPart>>> + Send>>;
}

impl<F, Fut> ChannelHandler for F
where
    F: Fn(NativePayload, CancelFlag, SteerSlot) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<super::envelope::ContentPart>>> + Send + 'static,
{
    fn handle(
        &self,
        env: NativePayload,
        cancel: CancelFlag,
        steer: SteerSlot,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<super::envelope::ContentPart>>> + Send>> {
        Box::pin((self)(env, cancel, steer))
    }
}

impl ChannelManager {
    pub fn start<H>(cfg: ChannelsConfig, router: SessionRouter, handler: H) -> Self
    where
        H: ChannelHandler,
    {
        let (ingest_tx, ingest_rx) = mpsc::channel::<NativePayload>(256);
        let inner = Arc::new(Mutex::new(Inner {
            cfg,
            router,
            sessions: HashMap::new(),
        }));
        let mgr = Self {
            inner: inner.clone(),
            ingest_tx,
        };
        tokio::spawn(dispatch_loop(inner, ingest_rx, Arc::new(handler)));
        mgr
    }

    pub async fn ingest(&self, mut env: NativePayload) -> Result<IngestResult> {
        let ep = {
            let g = self.inner.lock().await;
            g.cfg
                .endpoint_for_payload(&env)
                .cloned()
                .or_else(|| g.cfg.endpoints.iter().find(|e| e.enabled).cloned())
        };
        if let Some(ep) = ep.as_ref() {
            match access::admit(ep, &env) {
                GateDecision::Deny(why) => {
                    if why == "allowlist" && !env.is_group() {
                        let zh = matches!(
                            super::progress::ImLocale::detect(&env.query_text()),
                            super::progress::ImLocale::Zh
                        );
                        let parts = super::outbound::reply_text(super::pair::hint(ep, zh));
                        let _ = super::outbound::deliver(Some(ep), &env, &parts).await;
                    }
                    return Ok(IngestResult {
                        session_id: String::new(),
                        denied: Some(why),
                    });
                }
                GateDecision::Paired => {
                    let zh = matches!(
                        super::progress::ImLocale::detect(&env.query_text()),
                        super::progress::ImLocale::Zh
                    );
                    let msg = if zh {
                        "已绑定，直接发消息即可。"
                    } else {
                        "Paired. Send a message when you are ready."
                    };
                    let _ =
                        super::outbound::deliver(Some(ep), &env, &super::outbound::reply_text(msg))
                            .await;
                    return Ok(IngestResult {
                        session_id: String::new(),
                        denied: None,
                    });
                }
                GateDecision::Allow => {}
            }
            if env.channel.is_empty() {
                env.channel = if ep.kind.is_empty() {
                    ep.id.clone()
                } else {
                    ep.kind.clone()
                };
            }
            env.meta
                .entry("endpoint_id")
                .or_insert_with(|| serde_json::Value::String(ep.id.clone()));
        }
        let session_id = {
            let mut g = self.inner.lock().await;
            match crate::slash::parse_slash(&env.query_text()) {
                Some(crate::slash::SlashCmd::New { .. }) => {
                    let id = crate::session::new_session_id();
                    g.router.bind(&env, id)?
                }
                Some(crate::slash::SlashCmd::Resume { query }) => {
                    let home = crate::config::Config::home_dir()?;
                    let sessions = home.join("sessions");
                    if let Some(hit) = crate::session::catalog::resolve(
                        &sessions,
                        query.as_deref().unwrap_or("latest"),
                    )? {
                        env.meta.insert(
                            "resumed_session".into(),
                            serde_json::Value::String(hit.id.clone()),
                        );
                        g.router.bind(&env, hit.id)?
                    } else {
                        env.meta.insert(
                            "resume_error".into(),
                            serde_json::Value::String("session not found".into()),
                        );
                        g.router.resolve(&env)?
                    }
                }
                _ => g.router.resolve(&env)?,
            }
        };
        env.session_id = session_id.clone();
        self.ingest_tx
            .send(env)
            .await
            .map_err(|_| crate::error::Error::msg("channel ingest closed"))?;
        Ok(IngestResult {
            session_id,
            denied: None,
        })
    }
}

async fn dispatch_loop<H: ChannelHandler>(
    inner: Arc<Mutex<Inner>>,
    mut rx: mpsc::Receiver<NativePayload>,
    handler: Arc<H>,
) {
    let mut pending: HashMap<String, Vec<NativePayload>> = HashMap::new();
    let mut ticks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<String>();

    loop {
        tokio::select! {
            env = rx.recv() => {
                // flush_tx 被本任务自己持有，flush_rx 永远不会关闭，原来的
                // else 分支永不触发。ingest 端全部丢弃（serve_endpoint 被
                // abort / manager 被丢弃）时这里拿到 None，直接退出。
                let Some(env) = env else { break };
                let key = env.session_id.clone();
                // 斜杠控制指令（/stop /steer /queue…）不等防抖：微信要等 3s
                // 才 cancel，且同窗口的「帮我改」+ /stop 会被 merge 成一段
                // 正文，parse_slash 要求整段以 / 开头，/stop 进了模型而任务
                // 不停。先按原序 flush 该会话已攒的普通消息，再直接投递。
                if env.query_text().trim_start().starts_with('/') {
                    flush_pending(&inner, &handler, &mut pending, &mut ticks, &key).await;
                    let tx = session_tx(&inner, &key, handler.clone()).await;
                    let _ = tx.send(env).await;
                    continue;
                }
                let wait = debounce_for(&env);
                pending.entry(key.clone()).or_default().push(env);
                if let Some(old) = ticks.remove(&key) {
                    old.abort();
                }
                let tx = flush_tx.clone();
                ticks.insert(key.clone(), tokio::spawn(async move {
                    sleep(wait).await;
                    let _ = tx.send(key);
                }));
            }
            Some(key) = flush_rx.recv() => {
                flush_pending(&inner, &handler, &mut pending, &mut ticks, &key).await;
            }
        }
    }
    for (_, tick) in ticks {
        tick.abort();
    }
}

/// 按到达顺序把一个会话的防抖缓存排进它的 worker。
async fn flush_pending<H: ChannelHandler>(
    inner: &Arc<Mutex<Inner>>,
    handler: &Arc<H>,
    pending: &mut HashMap<String, Vec<NativePayload>>,
    ticks: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    key: &str,
) {
    if let Some(old) = ticks.remove(key) {
        old.abort();
    }
    let Some(batch) = pending.remove(key) else {
        return;
    };
    for group in sender_batches(batch) {
        let Some(merged) = NativePayload::merge(group) else { continue };
        let tx = session_tx(inner, &merged.session_id, handler.clone()).await;
        let _ = tx.send(merged).await;
    }
}

/// Preserve arrival order but never concatenate two users in a shared group
/// session into one model turn.
fn sender_batches(batch: Vec<NativePayload>) -> Vec<Vec<NativePayload>> {
    let mut groups: Vec<Vec<NativePayload>> = Vec::new();
    for env in batch {
        let same_sender = groups
            .last()
            .and_then(|g| g.last())
            .is_some_and(|prev| prev.sender_id == env.sender_id);
        if same_sender {
            groups.last_mut().expect("last checked").push(env);
        } else {
            groups.push(vec![env]);
        }
    }
    groups
}

async fn session_tx<H: ChannelHandler>(
    inner: &Arc<Mutex<Inner>>,
    session_id: &str,
    handler: Arc<H>,
) -> mpsc::Sender<NativePayload> {
    let mut g = inner.lock().await;
    if let Some(tx) = g.sessions.get(session_id) {
        return tx.clone();
    }
    let (tx, rx) = mpsc::channel::<NativePayload>(QUEUE_CAP);
    let cfg = g.cfg.clone();
    g.sessions.insert(session_id.to_string(), tx.clone());
    drop(g);
    let inner = inner.clone();
    let sid = session_id.to_string();
    tokio::spawn(async move {
        session_worker(rx, handler, cfg).await;
        let mut g = inner.lock().await;
        g.sessions.remove(&sid);
    });
    tx
}

async fn session_worker<H: ChannelHandler>(
    mut rx: mpsc::Receiver<NativePayload>,
    handler: Arc<H>,
    cfg: ChannelsConfig,
) {
    let mut busy = cfg.busy_policy();
    let mut queued: VecDeque<NativePayload> = VecDeque::new();
    loop {
        let mut env = if let Some(next) = queued.pop_front() {
            next
        } else {
            match rx.recv().await {
                Some(e) => e,
                None => break,
            }
        };
        match live_control(&env) {
            Some(LiveControl::Busy(policy)) => {
                busy = policy.unwrap_or(busy);
                let ep = cfg.endpoint_for_payload(&env).cloned();
                spawn_ack(ep, env.clone(), busy_status(&env, busy));
                continue;
            }
            Some(LiveControl::Stop) => {
                let ep = cfg.endpoint_for_payload(&env).cloned();
                spawn_ack(ep, env.clone(), no_active_status(&env));
                continue;
            }
            Some(LiveControl::Queue(text)) | Some(LiveControl::Steer(text)) => {
                env = env.follow_up_text(text);
            }
            None => {}
        }
        let ep = cfg.endpoint_for_payload(&env).cloned();
        if env.is_choice_click() {
            match super::interaction::answer(&env) {
                super::interaction::Answer::Accepted(reply)
                | super::interaction::Answer::Rejected(reply) => {
                    spawn_ack(ep, env, reply);
                    continue;
                }
                super::interaction::Answer::None => continue,
            }
        }
        let started = Instant::now();
        if !im_acked(&env) {
            let ack = super::outbound::reply_text(ImLocale::detect(&env.query_text()).ack());
            if let Err(e) =
                super::outbound::deliver_progress_since(ep.as_ref(), &env, &ack, started).await
            {
                eprintln!("hyper channel ack: {e}");
            }
        }
        let cancel = CancelFlag::new();
        let steer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut live = if env.channel == "qq" {
            Some(super::outbound::spawn_live_presence(
                ep.clone(),
                env.clone(),
            ))
        } else {
            None
        };
        let work = handler.handle(env.clone(), cancel.clone(), steer.clone());
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                next = rx.recv() => {
                    let Some(mut next) = next else {
                        abort_live(live.take()).await;
                        match work.await {
                            Ok(parts) => {
                                deliver_final(ep.as_ref(), &env, &parts, started).await;
                            }
                            Err(e) => {
                                let parts = super::outbound::reply_text(format!("error: {e}"));
                                let _ = super::outbound::deliver_since(
                                    ep.as_ref(),
                                    &env,
                                    &parts,
                                    started,
                                )
                                .await;
                            }
                        }
                        return;
                    };
                    match live_control(&next) {
                        Some(LiveControl::Stop) => {
                            cancel.cancel();
                            abort_live(live.take()).await;
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            spawn_ack(ep2, next, stop_status(&env));
                            let _ = work.await;
                            break;
                        }
                        Some(LiveControl::Busy(policy)) => {
                            busy = policy.unwrap_or(busy);
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            spawn_ack(ep2, next.clone(), busy_status(&next, busy));
                            continue;
                        }
                        Some(LiveControl::Queue(text)) => {
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            let mut queued_env = next.follow_up_text(text);
                            let loc = ImLocale::detect(&queued_env.query_text());
                            if queued.len() < QUEUE_CAP {
                                spawn_ack(ep2, queued_env.clone(), loc.queue_ack());
                                mark_im_acked(&mut queued_env);
                                queued.push_back(queued_env);
                            } else {
                                spawn_ack(ep2, queued_env, loc.overflow_ack());
                            }
                            continue;
                        }
                        Some(LiveControl::Steer(text)) => {
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            push_steer(&steer, text);
                            spawn_ack(ep2, next.clone(), ImLocale::detect(&next.query_text()).steer_ack());
                            continue;
                        }
                        None => {}
                    }
                    match super::interaction::mid_turn(&next) {
                        super::interaction::MidTurn::Reply(reply) => {
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            spawn_ack(ep2, next, reply);
                            continue;
                        }
                        super::interaction::MidTurn::Drop => continue,
                        super::interaction::MidTurn::Pass => {}
                    }
                    match busy {
                        BusyPolicy::Interrupt => {
                            cancel.cancel();
                            abort_live(live.take()).await;
                            let _ = work.await;
                            queued.push_front(next);
                            break;
                        }
                        BusyPolicy::Queue => {
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            let loc = ImLocale::detect(&next.query_text());
                            if queued.len() < QUEUE_CAP {
                                spawn_ack(ep2, next.clone(), loc.queue_ack());
                                mark_im_acked(&mut next);
                                queued.push_back(next);
                            } else {
                                spawn_ack(ep2, next, loc.overflow_ack());
                            }
                        }
                        BusyPolicy::Steer => {
                            let ep2 = cfg.endpoint_for_payload(&next).cloned();
                            let loc = ImLocale::detect(&next.query_text());
                            let text = next.query_text();
                            if !text.trim().is_empty() {
                                push_steer(&steer, text);
                                spawn_ack(ep2.clone(), next.clone(), loc.steer_ack());
                            }
                            if !next.media_parts().is_empty() {
                                if queued.len() < QUEUE_CAP {
                                    spawn_ack(ep2, next.clone(), loc.queue_ack());
                                    mark_im_acked(&mut next);
                                    queued.push_back(next);
                                } else {
                                    spawn_ack(ep2, next, loc.overflow_ack());
                                }
                            }
                        }
                    }
                }
                result = &mut work => {
                    abort_live(live.take()).await;
                    match result {
                        Ok(parts) => {
                            deliver_final(ep.as_ref(), &env, &parts, started).await;
                        }
                        Err(e) => {
                            let s = e.to_string();
                            if s != "aborted" && !s.contains("aborted") {
                                let parts = super::outbound::reply_text(format!("error: {e}"));
                                let _ = super::outbound::deliver_since(
                                    ep.as_ref(),
                                    &env,
                                    &parts,
                                    started,
                                )
                                .await;
                            }
                        }
                    }
                    // Steer notes that missed the last tool boundary stay in
                    // the slot (Agent::finish → pending_steer → restore).
                    // Sidecar queues them as the next turn; IM must too.
                    if !cancel.is_cancelled() {
                        enqueue_leftover_steer(&env, take_steer(&steer), &mut queued);
                    }
                    break;
                }
            }
        }
    }
}

async fn abort_live(live: Option<tokio::task::JoinHandle<()>>) {
    let Some(live) = live else {
        return;
    };
    live.abort();
    let _ = live.await;
}

enum LiveControl {
    Stop,
    Queue(String),
    Steer(String),
    Busy(Option<BusyPolicy>),
}

fn live_control(env: &NativePayload) -> Option<LiveControl> {
    match crate::slash::parse_slash(&env.query_text())? {
        crate::slash::SlashCmd::Stop => Some(LiveControl::Stop),
        crate::slash::SlashCmd::Queue { text } => Some(LiveControl::Queue(text)),
        crate::slash::SlashCmd::Steer { text } => Some(LiveControl::Steer(text)),
        crate::slash::SlashCmd::Busy { policy } => Some(LiveControl::Busy(policy)),
        _ => None,
    }
}

fn stop_status(env: &NativePayload) -> &'static str {
    match ImLocale::detect(&env.query_text()) {
        ImLocale::Zh => "已停止当前任务。",
        ImLocale::En => "Stopped the active task.",
    }
}

fn no_active_status(env: &NativePayload) -> &'static str {
    match ImLocale::detect(&env.query_text()) {
        ImLocale::Zh => "当前没有正在运行的任务。",
        ImLocale::En => "There is no active task.",
    }
}

fn busy_status(env: &NativePayload, busy: BusyPolicy) -> String {
    match ImLocale::detect(&env.query_text()) {
        ImLocale::Zh => format!("当前忙时策略：{}。", busy.as_str()),
        ImLocale::En => format!("Busy policy: {}.", busy.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_tokens_suppress_outbound() {
        use super::super::outbound::reply_text;
        assert!(is_silence(&reply_text("NO_REPLY")));
        assert!(is_silence(&reply_text("[silent]")));
        assert!(is_silence(&reply_text(" no reply \n")));
        assert!(is_silence(&reply_text("[NO_REPLY]")));
        // 尾标点不破坏沉默判定；空括号不算沉默。
        assert!(is_silence(&reply_text("NO_REPLY。")));
        assert!(is_silence(&reply_text("NO_REPLY.")));
        assert!(is_silence(&reply_text("[NO_REPLY]。")));
        assert!(!is_silence(&reply_text("[]")));
        assert!(!is_silence(&reply_text("好的，已修改。")));
        assert!(!is_silence(&reply_text("NO_REPLY 是什么意思？")));
        let mut with_media = reply_text("NO_REPLY");
        with_media.push(super::super::envelope::ContentPart::Image {
            image_url: "https://x/i.png".into(),
            url: String::new(),
            mime: String::new(),
        });
        assert!(!is_silence(&with_media), "attachments are real content");
        assert!(!is_silence(&[]));
    }

    #[test]
    fn debounce_is_per_channel() {
        let qq = NativePayload::text_only("qq", "在吗");
        assert_eq!(debounce_for(&qq), Duration::from_millis(1000));
        let wechat = NativePayload::text_only("wechat", "在吗");
        assert_eq!(debounce_for(&wechat), Duration::from_millis(3000));
        let feishu = NativePayload::text_only("feishu", "在吗");
        assert_eq!(debounce_for(&feishu), Duration::from_millis(600));
        let mut feishu_media = NativePayload::text_only("feishu", "看图");
        feishu_media
            .content_parts
            .push(super::super::envelope::ContentPart::Image {
                image_url: "https://x/i.png".into(),
                url: String::new(),
                mime: String::new(),
            });
        assert_eq!(debounce_for(&feishu_media), Duration::from_millis(800));
        let hook = NativePayload::text_only("webhook", "hi");
        assert_eq!(debounce_for(&hook), DEBOUNCE);
    }

    #[test]
    fn debounce_never_merges_adjacent_messages_from_different_senders() {
        let mut a1 = NativePayload::text_only("feishu", "a1");
        a1.sender_id = "alice".into();
        let mut a2 = NativePayload::text_only("feishu", "a2");
        a2.sender_id = "alice".into();
        let mut b = NativePayload::text_only("feishu", "b");
        b.sender_id = "bob".into();
        let groups = sender_batches(vec![a1, a2, b]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[0][0].sender_id, "alice");
        assert_eq!(groups[1][0].sender_id, "bob");
    }

    #[tokio::test]
    async fn dispatch_loop_exits_when_ingest_side_drops() {
        let dir =
            std::env::temp_dir().join(format!("hyper-chan-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let inner = Arc::new(Mutex::new(Inner {
            cfg: ChannelsConfig::default(),
            router,
            sessions: HashMap::new(),
        }));
        let (tx, rx) = mpsc::channel::<NativePayload>(4);
        let handler = Arc::new(
            |_env: NativePayload, _cancel: crate::tool_calls::CancelFlag, _steer: SteerSlot| async move {
                Ok(Vec::<super::super::envelope::ContentPart>::new())
            },
        );
        let task = tokio::spawn(dispatch_loop(inner, rx, handler));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("dispatch_loop must exit once all ingest senders drop")
            .unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn slash_control_skips_debounce() {
        let dir =
            std::env::temp_dir().join(format!("hyper-slash-dbc-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled_h = cancelled.clone();
        let mgr = ChannelManager::start(
            ChannelsConfig::default(),
            router,
            move |_env, cancel: crate::tool_calls::CancelFlag, _steer| {
                let cancelled_h = cancelled_h.clone();
                async move {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !cancel.is_cancelled() && Instant::now() < deadline {
                        sleep(Duration::from_millis(20)).await;
                    }
                    cancelled_h.store(cancel.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
                    Ok(Vec::new())
                }
            },
        );
        // 微信防抖 3s：先发的普通消息进 pending，/stop 必须绕开防抖直接
        // cancel，而不是等窗口或和「帮我改」merge 成一段正文。
        let first = mgr
            .ingest(NativePayload::text_only("wechat", "帮我改"))
            .await
            .unwrap();
        assert!(first.denied.is_none());
        sleep(Duration::from_millis(100)).await;
        let mut stop = NativePayload::text_only("wechat", "/stop");
        stop.session_id = first.session_id.clone();
        mgr.ingest(stop).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cancelled.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(50)).await;
        }
        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "/stop must cancel the live turn inside the 3s wechat debounce window"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn steer_injects_into_live_slot_not_a_second_turn() {
        let dir =
            std::env::temp_dir().join(format!("hyper-steer-im-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let cfg = ChannelsConfig {
            busy: "steer".into(),
            ..ChannelsConfig::default()
        };
        let turns = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let got = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let turns_h = turns.clone();
        let got_h = got.clone();
        let mgr = ChannelManager::start(cfg, router, move |_env, _cancel, steer| {
            let turns_h = turns_h.clone();
            let got_h = got_h.clone();
            let started_tx = started_tx.clone();
            async move {
                let n = turns_h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    if let Some(tx) = started_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    let deadline = Instant::now() + Duration::from_secs(3);
                    loop {
                        let notes = crate::channel::take_steer(&steer);
                        if !notes.is_empty() {
                            *got_h.lock().unwrap() = notes;
                            break;
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                        sleep(Duration::from_millis(20)).await;
                    }
                }
                Ok(Vec::new())
            }
        });
        let first = mgr
            .ingest(NativePayload::text_only("webhook", "do the long task"))
            .await
            .unwrap();
        assert!(first.denied.is_none());
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("first turn started")
            .unwrap();
        let mut follow = NativePayload::text_only("webhook", "focus auth.rs");
        follow.session_id = first.session_id.clone();
        mgr.ingest(follow).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !got.lock().unwrap().is_empty() {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            turns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "steer must not start a second agent turn"
        );
        let notes = got.lock().unwrap().clone();
        assert!(notes.iter().any(|s| s.contains("focus auth")), "{notes:?}");
        drop(mgr);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn leftover_steer_after_final_hop_starts_follow_up_turn() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-steer-leftover-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let cfg = ChannelsConfig::default();
        let turns = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let second = Arc::new(std::sync::Mutex::new(String::new()));
        let turns_h = turns.clone();
        let second_h = second.clone();
        let mgr = ChannelManager::start(cfg, router, move |env: NativePayload, _cancel, steer| {
            let turns_h = turns_h.clone();
            let second_h = second_h.clone();
            async move {
                let n = turns_h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    push_steer(&steer, "also add tests".into());
                } else {
                    *second_h.lock().unwrap() = env.query_text();
                }
                Ok(Vec::new())
            }
        });
        mgr.ingest(NativePayload::text_only("webhook", "do the long task"))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if turns.load(std::sync::atomic::Ordering::SeqCst) >= 2
                && !second.lock().unwrap().is_empty()
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            turns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "leftover steer after the last hop must start a follow-up turn"
        );
        assert!(
            second.lock().unwrap().contains("also add tests"),
            "{:?}",
            second.lock().unwrap()
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn english_inbound_acks_in_english() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                    _ => break,
                }
                if let Some(i) = find_double_crlf(&buf) {
                    let headers = String::from_utf8_lossy(&buf[..i]);
                    let want = content_length(&headers);
                    if buf.len() >= i + 4 + want {
                        let body = String::from_utf8_lossy(&buf[i + 4..i + 4 + want]).into_owned();
                        let _ = tx.send(body);
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        return;
                    }
                }
            }
        });
        let dir =
            std::env::temp_dir().join(format!("hyper-im-lang-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let mgr = ChannelManager::start(ChannelsConfig::default(), router, |_env, _c, _s| async {
            sleep(Duration::from_millis(400)).await;
            Ok(Vec::new())
        });
        let mut env = NativePayload::text_only("webhook", "please fix the title in Chat.tsx");
        env.meta.insert(
            "reply_url".into(),
            serde_json::json!(format!("http://{addr}/")),
        );
        mgr.ingest(env).await.unwrap();
        let body = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("ack posted")
            .expect("ack body");
        assert!(
            body.contains("Got it, working on it") || body.contains("Got it"),
            "english inbound must ACK in English: {body}"
        );
        assert!(
            !body.contains("收到"),
            "must not fall back to Chinese ACK: {body}"
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn chinese_queue_ack_while_turn_runs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                        _ => break,
                    }
                    if let Some(i) = find_double_crlf(&buf) {
                        let headers = String::from_utf8_lossy(&buf[..i]);
                        let want = content_length(&headers);
                        if buf.len() >= i + 4 + want {
                            let body =
                                String::from_utf8_lossy(&buf[i + 4..i + 4 + want]).into_owned();
                            let _ = tx.send(body);
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                                .await;
                            break;
                        }
                    }
                }
            }
        });
        let dir =
            std::env::temp_dir().join(format!("hyper-im-qack-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let turns = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let turns_h = turns.clone();
        let mgr = ChannelManager::start(
            ChannelsConfig {
                busy: "queue".into(),
                ..ChannelsConfig::default()
            },
            router,
            move |_env, _c, _s| {
                let turns_h = turns_h.clone();
                let started_tx = started_tx.clone();
                async move {
                    let n = turns_h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n == 0 {
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        sleep(Duration::from_millis(1500)).await;
                    }
                    Ok(Vec::new())
                }
            },
        );
        let reply = format!("http://{addr}/");
        let mut first = NativePayload::text_only("webhook", "先写一个文件，中文回复。");
        first.session_id = "queue-ack-sid".into();
        first
            .meta
            .insert("reply_url".into(), serde_json::json!(reply.clone()));
        mgr.ingest(first).await.unwrap();
        started_rx.await.expect("first turn started");
        let mut follow = NativePayload::text_only("webhook", "再追加一行注释。");
        follow.session_id = "queue-ack-sid".into();
        follow
            .meta
            .insert("reply_url".into(), serde_json::json!(reply));
        mgr.ingest(follow).await.unwrap();
        let mut blob = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(400), rx.recv()).await {
                Ok(Some(body)) => {
                    blob.push_str(&body);
                    blob.push('\n');
                    if blob.contains("收到，正在处理") && blob.contains("当前任务结束后回复")
                    {
                        break;
                    }
                }
                _ => {
                    if turns.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                        break;
                    }
                }
            }
        }
        assert!(blob.contains("收到，正在处理"), "first ACK missing: {blob}");
        assert!(
            blob.contains("当前任务结束后回复"),
            "queue ACK missing: {blob}"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && turns.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            turns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "queued follow-up must run as a second turn"
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn abort_live_cancels_before_next_await() {
        let live = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let start = Instant::now();
        abort_live(Some(live)).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "QQ typing/heartbeat must stop as soon as the turn ends"
        );
        abort_live(None).await;
    }

    fn find_double_crlf(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("content-length") {
                    v.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }
}

fn enqueue_leftover_steer(
    origin: &NativePayload,
    notes: Vec<String>,
    queued: &mut VecDeque<NativePayload>,
) {
    for note in notes.into_iter().rev() {
        if note.trim().is_empty() {
            continue;
        }
        if queued.len() >= QUEUE_CAP {
            break;
        }
        let mut follow = origin.follow_up_text(note);
        mark_im_acked(&mut follow);
        queued.push_front(follow);
    }
}

fn im_acked(env: &NativePayload) -> bool {
    env.meta
        .get("im_acked")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn mark_im_acked(env: &mut NativePayload) {
    env.meta.insert("im_acked".into(), serde_json::json!(true));
}

fn spawn_ack(
    ep: Option<super::ChannelEndpoint>,
    env: NativePayload,
    text: impl Into<String> + Send + 'static,
) {
    let text = text.into();
    tokio::spawn(async move {
        let parts = super::outbound::reply_text(text);
        let _ = super::outbound::deliver_since(ep.as_ref(), &env, &parts, Instant::now()).await;
    });
}

/// Hermes intentional-silence tokens: when the agent's whole reply is
/// NO_REPLY / SILENT it read the chat and chose not to speak. The turn stays
/// in the transcript; only the outbound message is held back. Errors still
/// report through the `Err` path, and the wrap-up contract is untouched
/// because the reply text is non-empty.
fn is_silence(parts: &[super::envelope::ContentPart]) -> bool {
    if parts.is_empty() || parts.iter().any(|p| p.as_text().is_none()) {
        // Media attachments are real content, never a silence token.
        return false;
    }
    let text = super::outbound::parts_to_text(parts);
    let norm = super::normalize_silence(&text);
    // 空串（比如整段只有 "[]"）不是沉默。
    !norm.is_empty() && super::is_silence_token(&norm)
}

async fn deliver_final(
    ep: Option<&super::ChannelEndpoint>,
    env: &NativePayload,
    parts: &[super::envelope::ContentPart],
    started: Instant,
) {
    if is_silence(parts) {
        return;
    }
    if let Err(e) = super::outbound::deliver_since(ep, env, parts, started).await {
        eprintln!("hyper channel deliver: {e}");
    }
}
