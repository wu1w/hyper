import { applyHistoryIncoming, nextLive, preferFresherHistory } from "./chat-live.ts";
import type { Clarify, Permit, SessionEvent, Snap } from "./api.ts";

export type LiveBuf = { think: string; content: string };
export type Transcript = { events: SessionEvent[]; live: LiveBuf };

export function emptyLive(): LiveBuf {
  return { think: "", content: "" };
}

export function applyDelta(live: LiveBuf, e: SessionEvent): LiveBuf {
  if (e.reset) {
    if (e.content_only) return { ...live, content: "" };
    return emptyLive();
  }
  if (e.channel === "reasoning") return { ...live, think: live.think + (e.text || "") };
  return { ...live, content: live.content + (e.text || "") };
}

export function modalForFocus<T extends { session?: string } | null>(item: T, focused?: string): T | null {
  if (!item || !item.session || !focused || item.session === focused) return item;
  return null;
}

export function lastAssistantDup(events: SessionEvent[], incoming: SessionEvent): boolean {
  const body = (incoming.content || "").trim();
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].type !== "assistant") continue;
    if ((events[i].content || "") === (incoming.content || "") && body) return true;
    break;
  }
  return false;
}

export function ignoreHistoryReplace(
  p: { session?: string; events?: SessionEvent[]; reset?: boolean },
  focused?: string,
): boolean {
  return !!(p.session && focused && p.session !== focused && !p.events && !p.reset);
}

export type RpcMsg = { method: string; params: unknown };

export type RpcCtx = {
  session: () => string | undefined;
  transcripts: Record<string, Transcript>;
  setSnap: (s: Snap) => void;
  setEvents: (e: SessionEvent[] | ((xs: SessionEvent[]) => SessionEvent[])) => void;
  setLive: (l: LiveBuf | ((l: LiveBuf) => LiveBuf)) => void;
  setPermit: (p: Permit) => void;
  setClarify: (c: Clarify) => void;
  setPendingTurn: (v: boolean) => void;
  scheduleLive: (sid: string) => void;
  pullHistory: () => void;
  cancelLiveRaf: () => void;
};

function tape(ctx: RpcCtx, sid: string): Transcript {
  return ctx.transcripts[sid] || { events: [], live: emptyLive() };
}

export function handleHello(params: unknown, ctx: RpcCtx) {
  const p = params as {
    state?: Snap;
    events?: SessionEvent[];
    permit?: Permit;
    clarify?: Clarify;
  };
  const st = p.state || {};
  ctx.setSnap(st);
  if (p.events) ctx.setEvents(p.events);
  ctx.setPermit(modalForFocus(p.permit ?? null, st.session));
  ctx.setClarify(modalForFocus(p.clarify ?? null, st.session));
  ctx.cancelLiveRaf();
  ctx.setLive(emptyLive());
  ctx.setPendingTurn(false);
  if (st.session) {
    ctx.transcripts[st.session] = { events: p.events || [], live: emptyLive() };
  }
}

export function handleResync(params: unknown, ctx: RpcCtx) {
  const p = params as {
    state?: Snap;
    events?: SessionEvent[];
    permit?: Permit;
    clarify?: Clarify;
  };
  if (p.state) ctx.setSnap(p.state);
  const focused = p.state?.session || ctx.session();
  ctx.setPermit(modalForFocus(p.permit ?? null, focused));
  ctx.setClarify(modalForFocus(p.clarify ?? null, focused));
  if (Array.isArray(p.events)) {
    const incoming = p.events;
    const id = focused;
    const parked = id ? ctx.transcripts[id] : undefined;
    const next = preferFresherHistory(parked?.events || incoming, incoming);
    ctx.cancelLiveRaf();
    ctx.setEvents(next);
    ctx.setLive((l) => {
      const liveNext = nextLive(next, parked?.live || l);
      if (id) ctx.transcripts[id] = { events: next, live: liveNext };
      return liveNext;
    });
  } else {
    ctx.pullHistory();
  }
}

export function handleHistoryReplace(params: unknown, ctx: RpcCtx) {
  const p = params as {
    events?: SessionEvent[];
    refetch?: boolean;
    session?: string;
    reset?: boolean;
  };
  const focused = ctx.session();
  if (ignoreHistoryReplace(p, focused)) return;
  if (p.reset) {
    const incoming = p.events || [];
    const sid = p.session || focused || "";
    if (sid) ctx.transcripts[sid] = { events: incoming, live: emptyLive() };
    ctx.cancelLiveRaf();
    ctx.setEvents(incoming);
    ctx.setLive(emptyLive());
    ctx.setPendingTurn(false);
    return;
  }
  if (p.refetch || !p.events) {
    ctx.pullHistory();
    return;
  }
  const sid = p.session || focused || "";
  const parked = sid ? ctx.transcripts[sid] : undefined;
  const next = preferFresherHistory(parked?.events || p.events, p.events);
  const keep = parked?.live || emptyLive();
  const liveNext = nextLive(next, keep);
  if (sid) ctx.transcripts[sid] = { events: next, live: liveNext };
  if (p.session && focused && p.session !== focused) return;
  ctx.cancelLiveRaf();
  ctx.setEvents(next);
  ctx.setLive(liveNext);
}

export function handleEventAppend(e: SessionEvent, ctx: RpcCtx) {
  const focused = ctx.session();
  const sid = e.session || focused || "";
  const t = tape(ctx, sid);
  if (e.type === "delta") {
    t.live = applyDelta(t.live, e);
    ctx.transcripts[sid] = t;
    if (!e.session || !focused || e.session === focused) ctx.scheduleLive(sid);
    return;
  }
  if (e.type === "assistant") {
    const body = (e.content || "").trim();
    const dup = lastAssistantDup(t.events, e);
    if (!dup) t.events = [...t.events, e];
    // Empty assistant hops must not wipe the overlay: history may
    // still be catching up, and stop arrives before the body lands.
    if (body) t.live = emptyLive();
    ctx.transcripts[sid] = t;
    if (e.session && focused && e.session !== focused) return;
    if (body) {
      ctx.cancelLiveRaf();
      ctx.setLive(emptyLive());
    }
    if (!dup) {
      ctx.setEvents((xs) => (lastAssistantDup(xs, e) ? xs : [...xs, e]));
    }
    return;
  }
  t.events = [...t.events, e];
  if (e.type === "stop") t.live = { think: "", content: t.live.content };
  ctx.transcripts[sid] = t;
  if (e.session && focused && e.session !== focused) return;
  if (e.type === "stop") {
    ctx.setPendingTurn(false);
    // Keep streamed content until history covers it; drop leftover CoT
    // so idle turns do not keep a 思考 overlay.
    ctx.setLive((l) => (l.think ? { think: "", content: l.content } : l));
  }
  ctx.setEvents((xs) => [...xs, e]);
}

export function handleConsoleRpc(msg: RpcMsg, ctx: RpcCtx) {
  switch (msg.method) {
    case "hello":
      handleHello(msg.params, ctx);
      return;
    case "resync":
      handleResync(msg.params, ctx);
      return;
    case "history.replace":
      handleHistoryReplace(msg.params, ctx);
      return;
    case "event.append":
      handleEventAppend(msg.params as SessionEvent, ctx);
      return;
    case "permit.ask": {
      const p = msg.params as Permit;
      const focused = ctx.session();
      if (p && p.session && focused && p.session !== focused) return;
      ctx.setPermit(p);
      return;
    }
    case "permit.clear":
      ctx.setPermit(null);
      return;
    case "clarify.ask": {
      const p = msg.params as Clarify;
      const focused = ctx.session();
      if (p && p.session && focused && p.session !== focused) return;
      ctx.setClarify(p);
      return;
    }
    case "clarify.clear":
      ctx.setClarify(null);
      return;
    case "state":
      ctx.setSnap(msg.params as Snap);
      return;
    default:
      return;
  }
}

export function parkReload(
  prevSess: string | undefined,
  st: Snap,
  incoming: SessionEvent[],
  parked: Transcript | undefined,
): { events: SessionEvent[]; switched: boolean } {
  const switched = !!st.session && !!prevSess && st.session !== prevSess;
  const next = applyHistoryIncoming(parked?.events || incoming, incoming, switched);
  return { events: next, switched };
}

export function beginTurnLive(live: LiveBuf, hint: string): LiveBuf {
  return live.think || live.content ? live : { think: hint, content: "" };
}

export function failTurnLive(live: LiveBuf, isHint: (think: string) => boolean): LiveBuf {
  return isHint(live.think) && !live.content ? emptyLive() : live;
}
