import { useEffect, useRef, useState } from "react";
import { api, connectEvents, rpc, type Clarify, type Permit, type SessionEvent, type Snap } from "./api";
import {
  FOOT,
  NAV,
  TITLES,
  detailsOpenFromStore,
  linkChipClass,
  linkDotClass,
  modelLinkLabel,
  pageFromHash,
  type PageId,
} from "./app-nav";
import { KeepPane, Titlebar } from "./app-chrome";
import {
  beginTurnLive,
  emptyLive,
  failTurnLive,
  handleConsoleRpc,
  modalForFocus,
  parkReload,
  type RpcCtx,
  type Transcript,
} from "./app-session";
import { PREPARE_HINT, isPrepareHint, nextLive, preferFresherHistory, runPhase } from "./chat-live";
import { ChatPage, ClarifyModal, PermitModal, RunChip } from "./Chat";
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

function initialDetails(): boolean {
  return detailsOpenFromStore(
    localStorage.getItem("hyper.details.open"),
    window.matchMedia("(min-width: 1181px)").matches,
  );
}

export type { PageId };

export function App() {
  const [page, setPage] = useState<PageId>(() => pageFromHash(location.hash));
  const [seen, setSeen] = useState<Set<PageId>>(() => new Set(["chat", pageFromHash(location.hash)]));
  const [rail, setRail] = useState(false);
  const [details, setDetails] = useState(initialDetails);
  const [wsUp, setWsUp] = useState(true);
  const [snap, setSnap] = useState<Snap>({});
  const [events, setEvents] = useState<SessionEvent[]>([]);
  const [live, setLive] = useState(emptyLive);
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
  const [pendingTurn, setPendingTurn] = useState(false);

  const go = (id: PageId) => {
    setPage(id);
    history.replaceState(null, "", `#${id}`);
  };

  const onReload = async () => {
    const prevSess = sessionRef.current;
    const st = await api<Snap>("/state");
    setSnap(st);
    setPermit(modalForFocus(st.permit ?? null, st.session));
    setClarify(modalForFocus(st.clarify ?? null, st.session));
    const h = await api<{ events: SessionEvent[] }>("/history");
    const incoming = h.events || [];
    const parked = st.session ? transcriptsRef.current[st.session] : undefined;
    const next = parkReload(prevSess, st, incoming, parked);
    setEvents(next.events);
    setLive((l) => {
      const liveNext = next.switched ? emptyLive() : nextLive(next.events, parked?.live || l);
      if (st.session) transcriptsRef.current[st.session] = { events: next.events, live: liveNext };
      return liveNext;
    });
  };

  const busy = !!snap.turn_in_flight || pendingTurn;

  useEffect(() => {
    if (snap.turn_in_flight) setPendingTurn(false);
  }, [snap.turn_in_flight]);

  const beginTurn = () => {
    setPendingTurn(true);
    setLive((l) => {
      const next = beginTurnLive(l, PREPARE_HINT);
      const id = sessionRef.current;
      if (id) {
        const t = transcriptsRef.current[id] || { events: [], live: emptyLive() };
        transcriptsRef.current[id] = { ...t, live: next };
      }
      return next;
    });
  };

  const failTurn = () => {
    setPendingTurn(false);
    setLive((l) => {
      const next = failTurnLive(l, isPrepareHint);
      const id = sessionRef.current;
      if (id) {
        const t = transcriptsRef.current[id];
        if (t) transcriptsRef.current[id] = { ...t, live: next };
      }
      return next;
    });
  };

  const goChat = () => {
    const fromOther = page !== "chat";
    go("chat");
    if (fromOther && !busy) void onReload();
  };

  useEffect(() => {
    const onHash = () => setPage(pageFromHash(location.hash));
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
    let liveRaf = 0;
    let liveSid = "";
    const cancelLiveRaf = () => {
      if (!liveRaf) return;
      cancelAnimationFrame(liveRaf);
      liveRaf = 0;
    };
    const paintLive = (sid: string) => {
      const focused = sessionRef.current;
      if (!sid || !focused || sid !== focused) return;
      const t = transcriptsRef.current[sid];
      if (t) setLive(t.live);
    };
    const scheduleLive = (sid: string) => {
      liveSid = sid;
      if (liveRaf) return;
      liveRaf = requestAnimationFrame(() => {
        liveRaf = 0;
        paintLive(liveSid);
      });
    };
    const pullHistory = () => {
      window.clearTimeout(histTimer);
      histTimer = window.setTimeout(() => {
        void api<{ events: SessionEvent[] }>("/history")
          .then((h) => {
            const incoming = h.events || [];
            const id = sessionRef.current;
            const parked = id ? transcriptsRef.current[id] : undefined;
            const next = preferFresherHistory(parked?.events || incoming, incoming);
            cancelLiveRaf();
            setEvents(next);
            setLive((l) => {
              const liveNext = nextLive(next, parked?.live || l);
              if (id) transcriptsRef.current[id] = { events: next, live: liveNext };
              return liveNext;
            });
          })
          .catch(() => {
            /* keep the live transcript if history is briefly unavailable */
          });
      }, 80);
    };
    const ctx: RpcCtx = {
      session: () => sessionRef.current,
      transcripts: transcriptsRef.current,
      setSnap,
      setEvents,
      setLive,
      setPermit,
      setClarify,
      setPendingTurn,
      scheduleLive,
      pullHistory,
      cancelLiveRaf,
    };
    const stop = connectEvents((msg) => handleConsoleRpc(msg, ctx), (up) => setWsUp(up));
    return () => {
      cancelLiveRaf();
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
        <Titlebar pageTitle={TITLES[page]}>
          <RunChip
            phase={phase}
            elapsed={elapsed}
            queued={snap.queued ?? 0}
            steered={snap.steered ?? 0}
            onClick={() => goChat()}
          />
          <button
            type="button"
            className={linkChipClass(link.ok)}
            title={link.error || (linked ? "模型端点可达" : "点此检查模型连接")}
            onClick={() => go("settings")}
          >
            <span className={linkDotClass(link.ok)} />
            <span className="link-txt">
              {modelLinkLabel({ linked, probing: link.ok === null, model: modelLabel })}
            </span>
          </button>
        </Titlebar>
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
                onTurnBegin={beginTurn}
                onTurnFailed={failTurn}
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
