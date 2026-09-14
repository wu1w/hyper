import type { Uploaded } from "./api.ts";
import { isJunkPath } from "./artifacts.ts";
import type { TreeEntry } from "./tree-model.ts";

export function imeBusy(e: { nativeEvent: { isComposing?: boolean }; isComposing?: boolean; keyCode: number }) {
  return e.isComposing === true || e.nativeEvent.isComposing === true || e.keyCode === 229;
}

export function fuzzyScore(query: string, text: string): number | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  if (!q) return 0;
  const hit = t.indexOf(q);
  if (hit >= 0) return 2000 - hit * 3 - Math.max(0, t.length - q.length);
  let qi = 0;
  let score = 0;
  let run = 0;
  for (let i = 0; i < t.length && qi < q.length; i++) {
    if (t[i] === q[qi]) {
      run++;
      score += 8 + run * 4;
      qi++;
    } else run = 0;
  }
  return qi === q.length ? score : null;
}

export function fuzzyFiles(entries: TreeEntry[], query: string): TreeEntry[] {
  const q = query.trim().toLowerCase();
  const scored: Array<{ hit: TreeEntry; score: number }> = [];
  for (const hit of entries) {
    if (isJunkPath(hit.path) || isJunkPath(hit.name)) continue;
    let score: number | null;
    if (!q) score = (hit.dir ? 0 : 80) - Math.min(80, hit.path.length);
    else score = fuzzyScore(q, hit.path) ?? fuzzyScore(q, hit.name);
    if (score == null) continue;
    scored.push({ hit, score });
  }
  scored.sort((a, b) => b.score - a.score || a.hit.path.length - b.hit.path.length);
  return scored.slice(0, 12).map((s) => s.hit);
}

export function mentionToken(
  text: string,
  cursor: number,
): { start: number; end: number; query: string } | null {
  if (text.startsWith("/")) return null;
  const at = text.slice(0, cursor).lastIndexOf("@");
  if (at < 0) return null;
  if (/\s/.test(text.slice(at + 1, cursor))) return null;
  if (at > 0 && !/[\s(\[{,:;'"`、]/.test(text[at - 1])) return null;
  let end = cursor;
  while (end < text.length && !/\s/.test(text[end])) end++;
  return { start: at, end, query: text.slice(at + 1, end) };
}

export function attachPayload(raw: string, atts: Uploaded[]) {
  const parts: unknown[] = [];
  const notes: string[] = [];
  for (const f of atts) {
    if (f.content_part && (f.kind === "image" || f.kind === "video" || f.kind === "audio")) {
      parts.push(f.content_part);
    } else notes.push(f.path);
  }
  let prompt = raw;
  if (notes.length) prompt = `${prompt ? prompt + "\n\n" : ""}${notes.map((p) => `[attached: ${p}]`).join("\n")}`;
  return { prompt: prompt || " ", content_parts: parts };
}

export function editorPayload(previewPath: string) {
  const path = previewPath.trim().replace(/^\/+/, "");
  if (!path) return { files: [] as { path: string }[] };
  return { active: path, files: [{ path }] };
}

export function insertAtCaret(
  current: string,
  insert: string,
  start: number,
  end: number,
): { next: string; caret: number } {
  const next = current.slice(0, start) + insert + current.slice(end);
  return { next, caret: start + insert.length };
}

const HIDE_OPEN = "<tool_response>";
const HIDE_CLOSE = "</tool_response>";

export function isHarnessNote(s: string): boolean {
  return (
    /\[(trajectory|locate|out|web|doc-read|oracle|baseline|style|cron|compact|verify:numeric|guard|background)\b/i.test(
      s,
    ) ||
    s.startsWith("HYPER_WORKING_WINDOW=") ||
    /^MEMORY(\.md| hot| hosts)/.test(s)
  );
}

export function hiddenNote(text: string): string | null {
  const t = text.trim();
  if (!t.startsWith(HIDE_OPEN) || !t.endsWith(HIDE_CLOSE)) return null;
  const inner = t
    .slice(HIDE_OPEN.length, Math.max(HIDE_OPEN.length, t.length - HIDE_CLOSE.length))
    .trim();
  if (isHarnessNote(inner)) return "";
  return inner;
}

export function firstLine(s: string): string {
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (t) return t;
  }
  return "";
}

export function clipEnd(s: string, n: number): string {
  const cs = [...s];
  return cs.length <= n ? s : `${cs.slice(0, n - 1).join("")}…`;
}

export function thinkTail(s: string): string {
  const t = s.trimEnd();
  const nl = t.lastIndexOf("\n");
  const line = (nl >= 0 ? t.slice(nl + 1) : t).trim();
  const cs = [...line];
  return cs.length <= 64 ? line : `…${cs.slice(-63).join("")}`;
}

export function pickAgentStatus(rpc?: string | null, card?: string, eventCount = 0): string {
  if (rpc === "running") return "running";
  if (card === "failed" || card === "cancelled") return card;
  if (rpc) return rpc;
  if (card) return card;
  return "done";
}

export function quietStopReason(reason: string): boolean {
  const r = reason.trim();
  if (!r || r === "stop" || r === "done" || r === "end_turn") return true;
  if (r === "parse failed") return true;
  if (r.startsWith("budget:")) return true;
  if (r.includes("Max iterations")) return true;
  if (r.includes("time limit")) return true;
  if (r.includes("Token budget")) return true;
  if (r.includes("call budget")) return true;
  return false;
}

export function stepStatusOf(done: boolean, output?: string): "run" | "ok" | "err" | "warn" {
  if (!done) return "run";
  const out = (output || "").trimStart();
  if (out === "tool task aborted") return "warn";
  if (/^(error|错误)/i.test(out)) return "err";
  return "ok";
}

export function fmtTokS(n: number): string {
  if (n >= 100) return `${Math.round(n)} tok/s`;
  if (n >= 10) return `${n.toFixed(1)} tok/s`;
  return `${n.toFixed(2)} tok/s`;
}

export function toolKey(name: string): string {
  return (name || "").toLowerCase().replace(/_/g, "");
}

export function toolBadge(name: string) {
  const n = toolKey(name);
  if (n === "read" || n === "view" || n === "recall" || n === "memorysearch") return "read";
  if (n === "edit" || n === "strreplace") return "edit";
  if (n === "write" || n === "delete") return "write";
  return "bash";
}

export function isTodoTool(name: string): boolean {
  const n = toolKey(name);
  return n === "todowrite" || n === "todo";
}

export function isTaskTool(name: string): boolean {
  const n = toolKey(name);
  return n === "task" || n === "spawnsubagent";
}

export function parseTaskOutput(out: string): { id?: string; status?: string } {
  const bg = out.match(/^BACKGROUND\s+(\S+)/m);
  if (bg) return { id: bg[1], status: "running" };
  const st = out.match(/^STATUS\s+(\S+)\s+id=(\S+)/m);
  if (st) return { status: st[1], id: st[2] };
  return {};
}

export function agentStatusLabel(status?: string): string {
  const s = (status || "").toLowerCase();
  if (s === "running") return "运行中";
  if (s === "done" || s === "completed") return "已完成";
  if (s === "failed") return "失败";
  if (s === "cancelled" || s === "canceled") return "已取消";
  return status || "";
}

export function isAskTool(name: string): boolean {
  const n = toolKey(name);
  return n === "askquestion" || n === "ask";
}

export function isHostImageTool(name: string): boolean {
  return toolKey(name) === "imagegeneration";
}

export function toolLabel(name: string): string {
  if (isHostImageTool(name)) return "生成图片";
  return name;
}

export function parseJsonObj(raw: string): Record<string, unknown> | null {
  try {
    const v = JSON.parse(raw || "{}") as unknown;
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

export function sendButtonLabel(busy: boolean, policy: string, imagine: boolean): string {
  if (!busy) return imagine ? "生成" : "发送";
  if (policy === "queue") return "排队";
  if (policy === "steer") return "转向";
  return "打断";
}

export function composerPlaceholder(busy: boolean, policy: string, imagine: boolean): string {
  if (imagine && !busy) return "描述要生成的图片… Enter 调用生图端点";
  if (!busy) return "给 grok-hyper 发消息…  / 唤起命令，粘贴图片走上传";
  if (policy === "queue") return "本轮结束后会跑这段话…";
  if (policy === "steer") return "下一个安全工具边界会吸收这段引导…";
  return "发送将打断当前轮次并改跑这段话…";
}

export function busyPolicyHint(policy: string): string {
  if (policy === "queue") return "忙碌策略 queue：Enter 排到本轮之后";
  if (policy === "steer") return "忙碌策略 steer：Enter 在下一个安全工具边界注入";
  return "忙碌策略 interrupt：Enter 打断本轮";
}

export function composerHintLine(busy: boolean, policy: string, imagine: boolean): string {
  if (busy) return `${busyPolicyHint(policy)} · 也可点停止 / 转向 / 排队`;
  if (imagine) return "Enter 生成图片 · Shift+Enter 换行";
  return "Enter 发送 · Shift+Enter 换行";
}

export type ComposerKeyPlan =
  | { act: "skip" }
  | { act: "clearSlash" }
  | { act: "clearMentions" }
  | { act: "navSlash"; dir: 1 | -1 }
  | { act: "navMentions"; dir: 1 | -1 }
  | { act: "applySlash" }
  | { act: "applyMention" }
  | { act: "enterSlash"; full: boolean }
  | { act: "enterMention" }
  | { act: "enterSend" };

export function composerKeyPlan(input: {
  composing: boolean;
  key: string;
  shiftKey: boolean;
  slashN: number;
  mentionN: number;
  typed: string;
  slashCmd?: string;
}): ComposerKeyPlan {
  if (input.composing) return { act: "skip" };
  if (input.slashN && input.key === "Escape") return { act: "clearSlash" };
  if (input.mentionN && input.key === "Escape") return { act: "clearMentions" };
  if (input.slashN && (input.key === "ArrowDown" || input.key === "ArrowUp")) {
    return { act: "navSlash", dir: input.key === "ArrowDown" ? 1 : -1 };
  }
  if (input.mentionN && (input.key === "ArrowDown" || input.key === "ArrowUp")) {
    return { act: "navMentions", dir: input.key === "ArrowDown" ? 1 : -1 };
  }
  if (input.slashN && input.key === "Tab") return { act: "applySlash" };
  if (input.mentionN && input.key === "Tab") return { act: "applyMention" };
  if (input.key === "Enter" && !input.shiftKey) {
    if (input.slashN) {
      const cmd = input.slashCmd || "";
      const typed = input.typed;
      return { act: "enterSlash", full: typed === cmd || typed.startsWith(`${cmd} `) };
    }
    if (input.mentionN) return { act: "enterMention" };
    return { act: "enterSend" };
  }
  return { act: "skip" };
}

export function detailsRailClass(opts: {
  open: boolean;
  tab: string;
  previewPath: string;
  previewMax: boolean;
}): string {
  const wide = opts.tab === "agent" || (!!opts.previewPath && opts.tab === "preview");
  return `details${opts.open ? "" : " closed"}${wide ? " wide" : ""}${opts.previewMax ? " pv-fill" : ""}${opts.tab === "agent" ? " agent-open" : ""}`;
}

export function detailsRailStyle(
  open: boolean,
  tab: string,
  previewPath: string,
  previewMax: boolean,
  railWidth: number,
): { width: number; flex: string } | undefined {
  if (open && (tab === "agent" || (previewPath && tab === "preview")) && !previewMax) {
    return { width: railWidth, flex: "0 0 auto" };
  }
  return undefined;
}

export function previewPathsOf(arts: string[], edited: string[], fallback: string[]): string[] {
  if (arts.length) return arts;
  if (edited.length) return edited;
  return fallback;
}

export function windowGauge(used: number, win: number): { rawPct: number; pct: number } {
  const rawPct = win ? Math.round((used / win) * 100) : 0;
  return { rawPct, pct: Math.min(100, Math.max(0, rawPct)) };
}

export function cacheHitLabel(reported: boolean, pct: number | null | undefined): string {
  return reported && pct != null ? `${pct.toFixed(1)}%` : "n/a";
}

export function waitPrefixBit(n: number): string {
  return n > 0 ? ` · ${n} tokens` : "";
}
