import type { SessionEvent } from "./api.ts";

export type LiveBuf = { think: string; content: string };

export function lastAssistantContent(events: SessionEvent[]): string {
  for (let i = events.length - 1; i >= 0; i--) {
    const spoken = spokenAssistant(events[i]);
    if (spoken) return spoken;
  }
  return "";
}

/**
 * Assistant text after the latest user turn. A previous turn's reply must not
 * count as covering the live buffer of the turn still on screen.
 */
export function lastAssistantInCurrentTurn(events: SessionEvent[]): string {
  let lastUser = -1;
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].type === "user") {
      lastUser = i;
      break;
    }
  }
  const start = lastUser + 1;
  for (let i = events.length - 1; i >= start; i--) {
    const spoken = spokenAssistant(events[i]);
    if (spoken) return spoken;
  }
  return "";
}

function spokenAssistant(e: SessionEvent): string {
  if (e.type !== "assistant") return "";
  if ((e.tool_calls?.length ?? 0) > 0) return "";
  return (e.content || "").trim();
}

/**
 * Keep the on-screen stream if the refetch is missing the just-finished reply.
 * Never use this across a session switch — empty incoming history would keep
 * the previous transcript on screen.
 */
export function preferFresherHistory(current: SessionEvent[], incoming: SessionEvent[]): SessionEvent[] {
  if (!incoming.length && current.length) return current;
  const a = lastAssistantInCurrentTurn(current);
  const b = lastAssistantInCurrentTurn(incoming);
  if (a && !b) return current;
  return incoming;
}

/** Session swap: take incoming even when it is empty. */
export function applyHistoryIncoming(
  current: SessionEvent[],
  incoming: SessionEvent[],
  reset: boolean,
): SessionEvent[] {
  return reset ? incoming : preferFresherHistory(current, incoming);
}

/**
 * True when committed events already contain the streamed answer, so the live
 * overlay can be dropped without a blank transcript.
 */
export function coversLive(events: SessionEvent[], live: LiveBuf): boolean {
  if (!live.content) {
    if (!live.think) return true;
    // Prepare hints are host chrome. Drop them once this turn already hopped,
    // so a history refetch does not resurrect「正在连接模型」between tools.
    if (isPrepareHint(live.think)) return turnHasHop(events);
    // Final reply already in events: leftover CoT overlay is not the answer.
    if (lastAssistantInCurrentTurn(events)) return true;
    return false;
  }
  const a = lastAssistantInCurrentTurn(events);
  if (!a) return false;
  if (a.includes(live.content)) return true;
  // Stream ran a few tokens ahead of the commit — keep the overlay.
  if (live.content.startsWith(a)) return false;
  return a.length > 0;
}

function turnHasHop(events: SessionEvent[]): boolean {
  let lastUser = -1;
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].type === "user") {
      lastUser = i;
      break;
    }
  }
  for (let i = lastUser + 1; i < events.length; i++) {
    const t = events[i].type;
    if (t === "assistant" || t === "tool") return true;
  }
  return false;
}

export function nextLive(events: SessionEvent[], live: LiveBuf): LiveBuf {
  return coversLive(events, live) ? { think: "", content: "" } : live;
}

/** Pulse think only before this hop has tools or spoken tokens. */
export function thinkOverlayLive(opts: { hasTools: boolean; hasContent: boolean }): boolean {
  return !opts.hasTools && !opts.hasContent;
}

function firstSentence(s: string): { head: string; rest: string } {
  const t = s.trim();
  const m = t.match(/^(.+?(?:。|！|？|\. |!\s|\?\s|\n))([\s\S]*)$/);
  if (!m) return { head: t, rest: "" };
  return { head: m[1].trim(), rest: m[2].trim() };
}

function fold(s: string): string {
  return s.replace(/\s+/g, " ").trim().toLowerCase();
}

/**
 * Hyper paints these before the first model token. Console `preparing` phase.
 */
export function isPrepareHint(think: string): boolean {
  return /^(正在整理上下文|正在准备工作区|正在连接模型)…?\s*$/.test(think.trim());
}

export const CONNECT_HINT = "正在连接模型…\n";
export const PREPARE_HINT = "正在准备工作区…\n";

export type RunPhase =
  | "idle"
  | "waiting"
  | "thinking"
  | "writing"
  | "tool"
  | "permit"
  | "clarify"
  | "stopping"
  | "retrying"
  | "preparing";

export function runPhase(opts: {
  busy: boolean;
  aborting?: boolean;
  live: LiveBuf;
  events: SessionEvent[];
  permit?: unknown;
  clarify?: unknown;
}): RunPhase {
  if (opts.clarify) return "clarify";
  if (opts.permit) return "permit";
  if (opts.aborting) return "stopping";
  if (!opts.busy) {
    // stop 往往早于最后一跳正文。思考 overlay 不能把芯片钉在「思考中」。
    if (opts.live.content) return "writing";
    return "idle";
  }
  const lifecycle = [...opts.events].reverse().find((event) =>
    event.type === "tool/lifecycle" || event.type === "step" || event.type === "run"
  );
  if (lifecycle?.type === "tool/lifecycle") {
    if (lifecycle.phase === "started") return "tool";
    if (lifecycle.phase === "error") return "retrying";
  }
  if (lifecycle?.type === "step" && lifecycle.phase === "started") {
    if (opts.live.think) return "thinking";
    return "waiting";
  }
  if (opts.live.content) return "writing";
  if (opts.live.think.includes("网络不稳") || opts.live.think.includes("正在重连")) return "retrying";
  if (isPrepareHint(opts.live.think)) return "preparing";
  if (opts.live.think) return "thinking";
  const last = opts.events[opts.events.length - 1];
  if (last?.type === "tool") return "tool";
  if (last?.type === "assistant" && (last.tool_calls?.length ?? 0) > 0) return "tool";
  return "waiting";
}

function isRestatementSentence(user: string, sentence: string): boolean {
  const u = user.trim();
  const s = sentence.trim();
  if (!u || !s) return false;
  if (s === u) return true;
  const sl = fold(s);
  const ul = fold(u);
  if (ul.length >= 4 && sl.includes(ul) && [...s].length < [...u].length + 48) return true;
  const prefixed =
    sl.startsWith("the user ") ||
    sl.startsWith("user wants") ||
    sl.startsWith("user asked") ||
    sl.startsWith("the task ") ||
    sl.startsWith("the user's ") ||
    s.startsWith("用户") ||
    s.startsWith("好的，用户");
  if (!prefixed) return false;
  if ([...s].length <= 96) return true;
  const chunk = ul.slice(0, 24);
  return chunk.length >= 4 && sl.includes(chunk);
}

/**
 * Drop a thinking preamble that only restates the user turn. Display-only;
 * the model never sees this.
 */
export function stripThinkRestatement(user: string, think: string): string {
  let rest = stripLeakedToolJson(think).trim();
  if (!user.trim() || !rest) return rest;
  for (let i = 0; i < 4 && rest; i++) {
    const { head, rest: tail } = firstSentence(rest);
    if (!isRestatementSentence(user, head)) break;
    rest = tail;
  }
  return rest;
}

const WRITE_TOOL_NAME =
  /"name"\s*:\s*"(Write|StrReplace|Delete|write|str_replace|strreplace|delete|Edit)"/;

/** Display-only: drop leaked Write/StrReplace JSON fences from the think panel. */
export function stripLeakedToolJson(think: string): string {
  if (!think) return think;
  let out = "";
  let i = 0;
  while (i < think.length) {
    const start = think.indexOf("```", i);
    if (start < 0) {
      out += holdBareWriteJson(think.slice(i));
      break;
    }
    out += think.slice(i, start);
    const after = start + 3;
    const nl = think.indexOf("\n", after);
    if (nl < 0) break;
    const lang = think.slice(after, nl).trim();
    const close = think.indexOf("```", nl + 1);
    if (close < 0) break;
    const inner = think.slice(nl + 1, close).trim();
    const jsonish = !lang || /^json$/i.test(lang);
    if (jsonish && WRITE_TOOL_NAME.test(inner) && (inner.startsWith("{") || inner.startsWith("["))) {
      i = close + 3;
      continue;
    }
    out += think.slice(start, close + 3);
    i = close + 3;
  }
  return out;
}

function holdBareWriteJson(tail: string): string {
  const trimmed = tail.trimEnd();
  const nl = trimmed.lastIndexOf("\n");
  const lineAt = nl < 0 ? 0 : nl + 1;
  const line = trimmed.slice(lineAt).trimStart();
  if (!(line.startsWith("{") || line.startsWith("["))) return tail;
  if (WRITE_TOOL_NAME.test(line)) return tail.slice(0, lineAt);
  return tail;
}

const TOOL_MARKUP = ["<tool_calls>", "<tool_call>", "<tool_results>", "<tool_result>"] as const;

/** Tags inside `…` or ```…``` are citations, not leaked markup. */
function inMarkdownCode(text: string, at: number): boolean {
  let fence = false;
  let inline = false;
  for (let i = 0; i < at; i++) {
    if (!inline && text.startsWith("```", i)) {
      fence = !fence;
      i += 2;
      continue;
    }
    if (!fence && text[i] === "`") inline = !inline;
  }
  return fence || inline;
}

function markupAt(text: string, open: string, from: number): number {
  let i = from;
  while (i < text.length) {
    const at = text.indexOf(open, i);
    if (at < 0) return -1;
    if (open === "<tool_call>" && text.startsWith("<tool_calls>", at)) {
      i = at + 1;
      continue;
    }
    if (open === "<tool_result>" && text.startsWith("<tool_results>", at)) {
      i = at + 1;
      continue;
    }
    if (inMarkdownCode(text, at)) {
      i = at + 1;
      continue;
    }
    return at;
  }
  return -1;
}

/** Cut leaked `<tool_calls>` / `<tool_result>` so they never paint as chat text. */
export function stripLeakedToolMarkup(text: string): string {
  if (!text) return text;
  let cut = -1;
  for (const open of TOOL_MARKUP) {
    const at = markupAt(text, open, 0);
    if (at < 0) continue;
    if (cut < 0 || at < cut) cut = at;
  }
  if (cut < 0) return text;
  return text.slice(0, cut).trimEnd();
}

export type DiffLine = { kind: "ctx" | "add" | "del"; text: string };

export type EditDiffView = {
  kind: "replace" | "write";
  path: string;
  lines: DiffLine[];
};

const DIFF_LINE_CAP = 240;

function capDiffLines(lines: DiffLine[]): DiffLine[] {
  if (lines.length <= DIFF_LINE_CAP) return lines;
  const extra = lines.length - DIFF_LINE_CAP;
  return [...lines.slice(0, DIFF_LINE_CAP), { kind: "ctx", text: `… ${extra} more lines` }];
}

/** Shared prefix/suffix hunk so StrReplace reads like Cursor, not a full Myers diff. */
export function editDiffLines(oldStr: string, newStr: string): DiffLine[] {
  const a = oldStr.split("\n");
  const b = newStr.split("\n");
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  let ja = a.length - 1;
  let jb = b.length - 1;
  while (ja >= i && jb >= i && a[ja] === b[jb]) {
    ja--;
    jb--;
  }
  const out: DiffLine[] = [];
  for (let k = 0; k < i; k++) out.push({ kind: "ctx", text: a[k] });
  for (let k = i; k <= ja; k++) out.push({ kind: "del", text: a[k] });
  for (let k = i; k <= jb; k++) out.push({ kind: "add", text: b[k] });
  for (let k = ja + 1; k < a.length; k++) out.push({ kind: "ctx", text: a[k] });
  return out;
}

export function editDiffFromTool(name: string, args: string): EditDiffView | null {
  const key = (name || "").toLowerCase().replace(/_/g, "");
  let obj: Record<string, unknown>;
  try {
    obj = JSON.parse(args || "{}") as Record<string, unknown>;
  } catch {
    return null;
  }
  if (!obj || typeof obj !== "object" || Array.isArray(obj)) return null;
  const str = (k: string) => (typeof obj[k] === "string" ? obj[k] : "");
  const path = str("path");
  if (key === "strreplace" || key === "edit") {
    if (!("old_string" in obj) && !("new_string" in obj)) return null;
    return {
      kind: "replace",
      path,
      lines: capDiffLines(editDiffLines(str("old_string"), str("new_string"))),
    };
  }
  if (key === "write") {
    if (!("contents" in obj) && !path) return null;
    const contents = str("contents");
    const lines = contents.split("\n").map((text) => ({ kind: "add" as const, text }));
    return { kind: "write", path, lines: capDiffLines(lines) };
  }
  return null;
}
