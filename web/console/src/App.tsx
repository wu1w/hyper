import { useEffect, useRef, useState, type ReactNode } from "react";
import { api, connectEvents, rpc, type Clarify, type Permit, type SessionEvent, type Snap } from "./api";

type LiveBuf = { think: string; content: string };
type Transcript = { events: SessionEvent[]; live: LiveBuf };

function emptyLive(): LiveBuf {
  return { think: "", content: "" };
}

function applyDelta(live: LiveBuf, e: SessionEvent): LiveBuf {
  if (e.reset) {
    if (e.content_only) return { ...live, content: "" };
    return emptyLive();
  }
  if (e.channel === "reasoning") return { ...live, think: live.think + (e.text || "") };
  return { ...live, content: live.content + (e.text || "") };
}

function modalForFocus<T extends { session?: string } | null>(item: T, focused?: string): T | null {
  if (!item || !item.session || !focused || item.session === focused) return item;
  return null;
}
import { ChatPage, ClarifyModal, PermitModal, RunChip, runPhase } from "./Chat";
import {
  ChannelsPage,
  CronPage,
  FilesPage,
  HeartbeatPage,
  InboxPage,
  McpPage,
  SecurityPage,
  SessionsPage,
  SettingsPage,
  SkillsPage,
  ToolsPage,
  UsagePage,
} from "./pages";
import { DialogHost, HoleMark, Icon } from "./ui";
import hyperWordmark from "./assets/hyper-wordmark.png";

function isWinDesktop() {
  return window.grokHyperDesktop?.platform === "win32";
}

function WinCaptionIcon({ kind }: { kind: "min" | "max" | "close" }) {
  if (kind === "min") {
    return (
      <svg viewBox="0 0 10 10" aria-hidden>
        <rect x="1" y="4.5" width="8" height="1" rx="0.2" />
      </svg>
    );
  }
  if (kind === "max") {
    return (
      <svg viewBox="0 0 10 10" aria-hidden>
        <rect x="1.5" y="1.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 10 10" aria-hidden>
      <path d="M2 2 L8 8 M8 2 L2 8" fill="none" stroke="currentColor" strokeWidth="1.15" />
    </svg>
  );
}

function WindowButtons() {
  const desktop = window.grokHyperDesktop;
  if (!desktop) {
    return (
      <div className="traffic" aria-hidden>
        <span className="tl-r" />
        <span className="tl-y" />
        <span className="tl-g" />
      </div>
    );
  }
  const win = desktop.platform === "win32";
  return (
    <div className="traffic">
      {win ? (
        <>
          <button type="button" className="tl-y" aria-label="最小化" onClick={() => desktop.minimize()}>
            <WinCaptionIcon kind="min" />
          </button>
          <button type="button" className="tl-g" aria-label="最大化" onClick={() => desktop.toggleMaximize()}>
            <WinCaptionIcon kind="max" />
          </button>
          <button type="button" className="tl-r" aria-label="关闭" onClick={() => desktop.close()}>
            <WinCaptionIcon kind="close" />
          </button>
        </>
      ) : (
        <>
          <button type="button" className="tl-r" aria-label="关闭" onClick={() => desktop.close()} />
          <button type="button" className="tl-y" aria-label="最小化" onClick={() => desktop.minimize()} />
          <button type="button" className="tl-g" aria-label="最大化" onClick={() => desktop.toggleMaximize()} />
        </>
      )}
    </div>
  );
}

/** 右侧状态栏默认态：宽屏展开，窄屏收起；用户手动切换后记住偏好。 */
function initialDetails(): boolean {
  const saved = localStorage.getItem("hyper.details.open");
  if (saved === "1") return true;
  if (saved === "0") return false;
  return window.matchMedia("(min-width: 1181px)").matches;
}

export type PageId =
  | "chat"
  | "inbox"
  | "channels"
  | "sessions"
  | "cron"
  | "heartbeat"
  | "files"
  | "skills"
  | "mcp"
  | "tools"
  | "settings"
  | "security"
  | "usage";

const NAV: Array<{ group: string; items: Array<{ id: PageId; label: string; icon: string; badge?: boolean }> }> = [
  {
    group: "主页",
    items: [
      { id: "chat", label: "聊天", icon: "chat" },
      { id: "channels", label: "频道", icon: "radio" },
      { id: "files", label: "文件", icon: "folder" },
      { id: "cron", label: "定时任务", icon: "clock" },
      { id: "heartbeat", label: "心跳", icon: "pulse" },
    ],
  },
  {
    group: "工作区",
    items: [
      { id: "inbox", label: "收件箱", icon: "shield", badge: true },
      { id: "sessions", label: "会话", icon: "list" },
      { id: "skills", label: "技能", icon: "spark" },
      { id: "mcp", label: "MCP", icon: "plug" },
      { id: "tools", label: "工具", icon: "wrench" },
    ],
  },
];

const FOOT: Array<{ id: PageId; label: string; icon: string }> = [
  { id: "settings", label: "模型", icon: "cpu" },
  { id: "security", label: "安全", icon: "lock" },
  { id: "usage", label: "用量", icon: "chart" },
];

const TITLES: Record<PageId, string> = {
  chat: "聊天",
  inbox: "收件箱",
  channels: "频道",
  sessions: "会话",
  cron: "定时任务",
  heartbeat: "心跳",
  files: "文件",
  skills: "技能",
  mcp: "MCP",
  tools: "工具",
  settings: "模型",
  security: "安全",
  usage: "用量",
};

function pageFromHash(): PageId {
  const h = location.hash.replace(/^#/, "") as PageId;
  if (h && TITLES[h]) return h;
  return "chat";
}

function KeepPane({
  id,
  page,
  seen,
  children,
}: {
  id: PageId;
  page: PageId;
  seen: Set<PageId>;
  children: ReactNode;
}) {
  if (page !== id && !seen.has(id)) return null;
  return (
    <div className="main-pane" hidden={page !== id} aria-hidden={page !== id}>
      {children}
    </div>
  );
}

export function App() {
  const [page, setPage] = useState<PageId>(pageFromHash);
  const [seen, setSeen] = useState<Set<PageId>>(() => new Set(["chat", pageFromHash()]));
  const [rail, setRail] = useState(false);
  const [details, setDetails] = useState(initialDetails);
  const [wsUp, setWsUp] = useState(true);
  const [snap, setSnap] = useState<Snap>({});
  const [events, setEvents] = useState<SessionEvent[]>([]);
  const [live, setLive] = useState({ think: "", content: "" });
  const [permit, setPermit] = useState<Permit>(null);
  const [clarify, setClarify] = useState<Clarify>(null);
  const [elapsed, setElapsed] = useState(0);
  const [link, setLink] = useState<{ ok: boolean | null; model: string; error?: string }>({
    ok: null,
    model: "",
  });
  const sessionRef = useRef(snap.session);
  sessionRef.current = snap.session;
  const transcriptsRef = useRef<Record<string, Transcript>>({});

  const go = (id: PageId) => {
    setPage(id);
    history.replaceState(null, "", `#${id}`);
  };

  const onReload = async () => {
    const st = await api<Snap>("/state");
    setSnap(st);
    setPermit(modalForFocus(st.permit ?? null, st.session));
    setClarify(modalForFocus(st.clarify ?? null, st.session));
    const h = await api<{ events: SessionEvent[] }>("/history");
    const next = h.events || [];
    setEvents(next);
    setLive(emptyLive());
    if (st.session) transcriptsRef.current[st.session] = { events: next, live: emptyLive() };
  };

  const busy = !!snap.turn_in_flight;

  const goChat = () => {
    const fromOther = page !== "chat";
    go("chat");
    if (fromOther && !busy) void onReload();
  };

  useEffect(() => {
    const onHash = () => setPage(pageFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    setSeen((s) => {
      if (s.has(page)) return s;
      const n = new Set(s);
      n.add(page);
      return n;
    });
  }, [page]);

  useEffect(() => {
    let histTimer: number | undefined;
    const pullHistory = (clearLive: boolean) => {
      window.clearTimeout(histTimer);
      histTimer = window.setTimeout(() => {
        void api<{ events: SessionEvent[] }>("/history")
          .then((h) => {
            const next = h.events || [];
            setEvents(next);
            if (clearLive) setLive(emptyLive());
            const id = sessionRef.current;
            if (id) {
              transcriptsRef.current[id] = {
                events: next,
                live: clearLive ? emptyLive() : transcriptsRef.current[id]?.live || emptyLive(),
              };
            }
          })
          .catch(() => {
            /* keep the live transcript if history is briefly unavailable */
          });
      }, 80);
    };
    const stop = connectEvents(
      (msg) => {
        if (msg.method === "hello") {
          const p = msg.params as {
            state?: Snap;
            events?: SessionEvent[];
            permit?: Permit;
            clarify?: Clarify;
          };
          const st = p.state || {};
          setSnap(st);
          if (p.events) setEvents(p.events);
          setPermit(modalForFocus(p.permit ?? null, st.session));
          setClarify(modalForFocus(p.clarify ?? null, st.session));
          setLive(emptyLive());
          if (st.session) {
            transcriptsRef.current[st.session] = { events: p.events || [], live: emptyLive() };
          }
        } else if (msg.method === "resync") {
          const p = msg.params as { state?: Snap; permit?: Permit; clarify?: Clarify };
          if (p.state) setSnap(p.state);
          const focused = p.state?.session || sessionRef.current;
          setPermit(modalForFocus(p.permit ?? null, focused));
          setClarify(modalForFocus(p.clarify ?? null, focused));
          pullHistory(false);
        } else if (msg.method === "history.replace") {
          const p = msg.params as { events?: SessionEvent[]; refetch?: boolean; session?: string };
          const focused = sessionRef.current;
          if (p.session && focused && p.session !== focused && !p.events) return;
          if (p.refetch || !p.events) pullHistory(true);
          else {
            const sid = p.session || focused || "";
            const keep = transcriptsRef.current[sid]?.live || emptyLive();
            if (sid) transcriptsRef.current[sid] = { events: p.events, live: keep };
            setEvents(p.events);
            setLive(keep.think || keep.content ? keep : emptyLive());
          }
        } else if (msg.method === "event.append") {
          const e = msg.params as SessionEvent;
          const focused = sessionRef.current;
          const sid = e.session || focused || "";
          const t = transcriptsRef.current[sid] || { events: [], live: emptyLive() };
          if (e.type === "delta") {
            t.live = applyDelta(t.live, e);
            transcriptsRef.current[sid] = t;
            if (!e.session || !focused || e.session === focused) setLive(t.live);
            return;
          }
          if (e.type === "assistant" || e.type === "stop") t.live = emptyLive();
          t.events = [...t.events, e];
          transcriptsRef.current[sid] = t;
          if (e.session && focused && e.session !== focused) return;
          if (e.type === "assistant" || e.type === "stop") setLive(emptyLive());
          setEvents((xs) => [...xs, e]);
        } else if (msg.method === "permit.ask") {
          const p = msg.params as Permit;
          const focused = sessionRef.current;
          if (p && p.session && focused && p.session !== focused) return;
          setPermit(p);
        } else if (msg.method === "permit.clear") {
          setPermit(null);
        } else if (msg.method === "clarify.ask") {
          const p = msg.params as Clarify;
          const focused = sessionRef.current;
          if (p && p.session && focused && p.session !== focused) return;
          setClarify(p);
        } else if (msg.method === "clarify.clear") {
          setClarify(null);
        } else if (msg.method === "state") {
          setSnap(msg.params as Snap);
        }
      },
      (up) => setWsUp(up),
    );
    return () => {
      window.clearTimeout(histTimer);
      stop();
    };
  }, []);

  const phase = runPhase({ busy, live, events, permit, clarify });
  const linked = link.ok === true;
  const modelLabel = (link.model || snap.model || "").trim();

  const toggleDetails = () =>
    setDetails((d) => {
      localStorage.setItem("hyper.details.open", d ? "0" : "1");
      return !d;
    });

  useEffect(() => {
    const id = snap.session;
    if (!id) return;
    const t = transcriptsRef.current[id];
    if (t) {
      setEvents(t.events);
      setLive(t.live);
    } else {
      setEvents([]);
      setLive(emptyLive());
    }
  }, [snap.session]);

  useEffect(() => {
    if (!busy) {
      setElapsed(0);
      return;
    }
    const started = snap.running_started?.[snap.session || ""] || Date.now();
    const tick = () => setElapsed(Math.max(0, Math.floor((Date.now() - started) / 1000)));
    tick();
    const id = setInterval(tick, 250);
    return () => clearInterval(id);
  }, [busy, snap.session, snap.running_started]);

  useEffect(() => {
    let stop = false;
    const tick = async () => {
      try {
        const j = await api<{ ok: boolean; model?: string; error?: string }>("/model");
        if (!stop) setLink({ ok: !!j.ok, model: j.model || "", error: j.error });
      } catch (e) {
        if (!stop) setLink((cur) => ({ ...cur, ok: false, error: String(e) }));
      }
    };
    tick();
    const id = setInterval(tick, 12000);
    return () => {
      stop = true;
      clearInterval(id);
    };
  }, [snap.model]);

  return (
    <>
      <div className="desktop" />
      <div className="window">
        <header
          className={`titlebar${isWinDesktop() ? " win" : ""}`}
          onDoubleClick={() => window.grokHyperDesktop?.toggleMaximize()}
        >
          {isWinDesktop() ? null : <WindowButtons />}
          <div className="titlebar-title">
            <span className="doc-title">grok-hyper 控制台</span>
            <span className="doc-sub"> · {TITLES[page]}</span>
          </div>
          <div className="spacer" />
          <div className="no-drag">
            <RunChip
              phase={phase}
              elapsed={elapsed}
              queued={snap.queued ?? 0}
              steered={snap.steered ?? 0}
              onClick={() => goChat()}
            />
            <button
              type="button"
              className={`chip link-chip${linked ? "" : link.ok === false ? " bad" : ""}`}
              title={link.error || (linked ? "模型端点可达" : "点此检查模型连接")}
              onClick={() => go("settings")}
            >
              <span className={`dot${linked ? "" : link.ok === null ? " wait" : " off"}`} />
              <span className="link-txt">
                {linked
                  ? modelLabel
                    ? `模型可达 · ${modelLabel}`
                    : "模型可达"
                  : link.ok === null
                    ? "检测中"
                    : "模型不可达"}
              </span>
            </button>
          </div>
          {isWinDesktop() ? <WindowButtons /> : null}
        </header>
        {!wsUp ? (
          <div className="ws-banner" role="alert">
            与 hyper 服务的连接已断开，正在自动重连… 若刚重启过服务，几秒内会自动恢复。
          </div>
        ) : null}
        <div className="body">
          <button
            type="button"
            className={`collapse-btn${rail ? " rail" : ""}`}
            title={rail ? "展开侧边栏" : "折叠侧边栏"}
            aria-label={rail ? "展开侧边栏" : "折叠侧边栏"}
            onClick={() => setRail(!rail)}
          >
            <Icon name={rail ? "chev-r" : "chev-l"} />
          </button>
          <aside className={`sidebar${rail ? " rail" : ""}`}>
            <div className="sb-head">
              <div className="wordmark">
                <HoleMark label="Hyper" />
                <div className="name">
                  <img className="wm-img" src={hyperWordmark} alt="Hyper" />
                  <small>Powered by Grok</small>
                </div>
              </div>
            </div>
            <button
              type="button"
              className="new-session"
              onClick={async () => {
                try {
                  await rpc("session.new", {});
                  go("chat");
                  await onReload();
                } catch {
                  /* keep the current chat if new session fails */
                }
              }}
            >
              <Icon name="plus" />
              <em>新建会话</em>
            </button>
            <div className="sb-scroll">
              {NAV.map((g) => (
                <div className="sb-section" key={g.group}>
                  <div className="sb-caption">{g.group}</div>
                  {g.items.map((it) => (
                    <button
                      key={it.id}
                      type="button"
                      className={`nav-item${page === it.id ? " on" : ""}${it.badge && (permit || clarify) ? " has-badge" : ""}`}
                      onClick={() => (it.id === "chat" ? goChat() : go(it.id))}
                    >
                      <Icon name={it.icon} />
                      <span className="txt">{it.label}</span>
                      {it.badge && (permit || clarify) ? <span className="badge">1</span> : null}
                    </button>
                  ))}
                </div>
              ))}
            </div>
            <div className="sb-foot">
              {FOOT.map((it) => (
                <button
                  key={it.id}
                  type="button"
                  className={`nav-item${page === it.id ? " on" : ""}`}
                  onClick={() => go(it.id)}
                >
                  <Icon name={it.icon} />
                  <span className="txt">{it.label}</span>
                </button>
              ))}
            </div>
          </aside>
          <div className="main">
            <KeepPane id="chat" page={page} seen={seen}>
              <ChatPage
                snap={snap}
                events={events}
                live={live}
                busy={busy}
                permit={permit}
                clarify={clarify}
                elapsed={elapsed}
                detailsOpen={details}
                onToggleDetails={toggleDetails}
                onReload={onReload}
              />
            </KeepPane>
            <KeepPane id="inbox" page={page} seen={seen}>
              <InboxPage permit={permit} clarify={clarify} onPermit={setPermit} />
            </KeepPane>
            <KeepPane id="channels" page={page} seen={seen}>
              <ChannelsPage active={page === "channels"} />
            </KeepPane>
            <KeepPane id="sessions" page={page} seen={seen}>
              <SessionsPage
                current={snap.session}
                running={snap.running}
                active={page === "sessions"}
                onOpen={goChat}
              />
            </KeepPane>
            <KeepPane id="cron" page={page} seen={seen}>
              <CronPage active={page === "cron"} busy={busy} />
            </KeepPane>
            <KeepPane id="heartbeat" page={page} seen={seen}>
              <HeartbeatPage active={page === "heartbeat"} busy={busy} />
            </KeepPane>
            <KeepPane id="files" page={page} seen={seen}>
              <FilesPage active={page === "files"} busy={busy} workspace={snap.workspace || ""} />
            </KeepPane>
            <KeepPane id="skills" page={page} seen={seen}>
              <SkillsPage active={page === "skills"} />
            </KeepPane>
            <KeepPane id="mcp" page={page} seen={seen}>
              <McpPage active={page === "mcp"} />
            </KeepPane>
            <KeepPane id="tools" page={page} seen={seen}>
              <ToolsPage active={page === "tools"} />
            </KeepPane>
            <KeepPane id="settings" page={page} seen={seen}>
              <SettingsPage active={page === "settings"} />
            </KeepPane>
            <KeepPane id="security" page={page} seen={seen}>
              <SecurityPage active={page === "security"} />
            </KeepPane>
            <KeepPane id="usage" page={page} seen={seen}>
              <UsagePage snap={snap} />
            </KeepPane>
          </div>
        </div>
      </div>
      {permit && page !== "inbox" ? <PermitModal permit={permit} onClose={() => setPermit(null)} /> : null}
      {clarify ? <ClarifyModal clarify={clarify} onClose={() => setClarify(null)} /> : null}
      <DialogHost />
    </>
  );
}
