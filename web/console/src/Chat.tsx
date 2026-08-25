import { memo, useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKey } from "react";
import {
  api,
  failMsg,
  nameFromEvents,
  rpc,
  sessionName,
  SLASH,
  usageCachedReported,
  usageCompacts,
  usageHitPct,
  usageLivePrompt,
  usageSteps,
  type Clarify,
  type Permit,
  type SessionEvent,
  type SessionInfo,
  type Snap,
  type Uploaded,
} from "./api";
import { isJunkPath, lastLiveUserIndex, turnArtifacts, turnPreviewPaths } from "./artifacts";
import { isOfficeKind, kindFor } from "./preview/kinds";
import { PreviewDock } from "./preview/PreviewDock";
import { MdText } from "./md";
import { Empty, Icon, Lightbox, Overlay, uiConfirm } from "./ui";
import {
  basename,
  clipboardPaste,
  fileFromDataUri,
  fileHref,
  ingestRemoteImages,
  isImageMedia,
  isImagePath,
  mediaSrc,
  parseStoredMedia,
  pathFromMediaUrl,
  stripAttachedNotes,
  userMediaFromEvent,
  type StoredMedia,
} from "./media";

/** IME confirm (Enter / 选词) must not send the message. keyCode 229 is the composition sentinel. */
function imeBusy(e: { nativeEvent: { isComposing?: boolean }; isComposing?: boolean; keyCode: number }) {
  return e.isComposing === true || e.nativeEvent.isComposing === true || e.keyCode === 229;
}

type FileHit = { path: string; name: string; dir: boolean };

function parseTreeEntry(raw: unknown): FileHit | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  const path = String(o.path ?? o.relative_path ?? o.rel ?? "")
    .replace(/\\/g, "/")
    .replace(/^\/+/, "");
  const name = String(o.name ?? o.file_name ?? o.filename ?? path.split("/").pop() ?? "");
  const dir = o.dir === true || o.is_dir === true || o.isDir === true || o.directory === true;
  const p = path || name;
  if (!p) return null;
  return { path: p, name: name || p.split("/").pop() || p, dir };
}

function fuzzyScore(query: string, text: string): number | null {
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

function fuzzyFiles(entries: FileHit[], query: string): FileHit[] {
  const q = query.trim().toLowerCase();
  const scored: Array<{ hit: FileHit; score: number }> = [];
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

/** `@token` at the cursor — not a leading slash command, token has no spaces. */
function mentionToken(text: string, cursor: number): { start: number; end: number; query: string } | null {
  if (text.startsWith("/")) return null;
  const at = text.slice(0, cursor).lastIndexOf("@");
  if (at < 0) return null;
  if (/\s/.test(text.slice(at + 1, cursor))) return null;
  if (at > 0 && !/[\s(\[{,:;'"`、]/.test(text[at - 1])) return null;
  let end = cursor;
  while (end < text.length && !/\s/.test(text[end])) end++;
  return { start: at, end, query: text.slice(at + 1, end) };
}

function attachPayload(raw: string, atts: Uploaded[]) {
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

function insertAtCaret(current: string, insert: string, start: number, end: number): { next: string; caret: number } {
  const next = current.slice(0, start) + insert + current.slice(end);
  return { next, caret: start + insert.length };
}

function MediaStrip({ items, onOpen }: { items: StoredMedia[]; onOpen?: (path: string) => void }) {
  if (!items.length) return null;
  return (
    <div className="media-strip">
      {items.map((m, i) => {
        const src = mediaSrc(m.url);
        const name = basename(m.url);
        const path = pathFromMediaUrl(m.url);
        const open = () => {
          if (path && onOpen) onOpen(path);
        };
        if (isImageMedia(m)) {
          return (
            <button key={`${src}:${i}`} type="button" className="att-card media" title={name} onClick={open}>
              <img className="att-shot" src={src} alt={name} />
            </button>
          );
        }
        if ((m.kind || "").toLowerCase() === "video") {
          return <video key={`${src}:${i}`} className="att-shot" src={src} muted playsInline />;
        }
        return (
          <button key={`${src}:${i}`} type="button" className="att-card file" title={name} onClick={open}>
            <Icon name="file" />
            <span>{name}</span>
          </button>
        );
      })}
    </div>
  );
}

function hasRecentAssistant(events: SessionEvent[]): boolean {
  const start = lastLiveUserIndex(events);
  for (let i = events.length - 1; i >= start; i--) {
    const e = events[i];
    if (e.type === "stop") {
      const r = String(e.reason || "").toLowerCase();
      if (r.includes("abort") || r.includes("cancel")) return false;
    }
    if (e.type === "assistant" && (String(e.content || "").trim() || parseStoredMedia(e.media).length)) {
      return true;
    }
  }
  return false;
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.left = "-9999px";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

function fmtTokS(n: number): string {
  if (n >= 100) return `${Math.round(n)} tok/s`;
  if (n >= 10) return `${n.toFixed(1)} tok/s`;
  return `${n.toFixed(2)} tok/s`;
}

function toolBadge(name: string) {
  const n = name.toLowerCase().replace(/_/g, "");
  if (n === "read" || n === "view" || n === "recall" || n === "memorysearch") return "read";
  if (n === "edit" || n === "strreplace") return "edit";
  if (n === "write" || n === "delete") return "write";
  return "bash";
}

function toolKey(name: string): string {
  return (name || "").toLowerCase().replace(/_/g, "");
}

function isTodoTool(name: string): boolean {
  const n = toolKey(name);
  return n === "todowrite" || n === "todo";
}

function isTaskTool(name: string): boolean {
  const n = toolKey(name);
  return n === "task" || n === "spawnsubagent";
}

function isAskTool(name: string): boolean {
  const n = toolKey(name);
  return n === "askquestion" || n === "ask";
}

function isHostImageTool(name: string): boolean {
  return toolKey(name) === "imagegeneration";
}

function toolLabel(name: string): string {
  if (isHostImageTool(name)) return "生成图片";
  return name;
}

type TodoItem = { id?: string; content: string; status?: string };

function parseJsonObj(raw: string): Record<string, unknown> | null {
  try {
    const v = JSON.parse(raw || "{}") as unknown;
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

function parseTodos(raw: string): TodoItem[] {
  const a = parseJsonObj(raw);
  const list = a?.todos ?? a?.items;
  if (!Array.isArray(list)) return [];
  return list
    .map((x) => {
      if (!x || typeof x !== "object") return null;
      const o = x as Record<string, unknown>;
      const content = typeof o.content === "string" ? o.content : typeof o.text === "string" ? o.text : "";
      if (!content) return null;
      return {
        id: typeof o.id === "string" ? o.id : undefined,
        content,
        status: typeof o.status === "string" ? o.status : "pending",
      };
    })
    .filter((x): x is TodoItem => !!x);
}

function strField(e: SessionEvent, ...keys: string[]): string | undefined {
  for (const k of keys) {
    const v = e[k];
    if (typeof v === "string" && v.trim()) return v;
  }
  return undefined;
}

function latestTodos(events: SessionEvent[]): TodoItem[] {
  let items: TodoItem[] = [];
  for (const e of events) {
    const t = (e.type || "").toLowerCase();
    if (t === "todo" || t === "todo_write" || t === "todos") {
      const fromEv = parseTodos(JSON.stringify(e));
      if (fromEv.length) items = fromEv;
    }
    for (const c of e.tool_calls || []) {
      if (isTodoTool(c.function?.name || "")) {
        const got = parseTodos(c.function?.arguments || "");
        if (got.length) items = got;
      }
    }
    if (e.type === "tool" && isTodoTool(e.name || "")) {
      const got = parseTodos(e.output || "");
      if (got.length) items = got;
    }
  }
  return items;
}

function subagentRows(events: SessionEvent[]): Array<{ id: string; label: string; status: string }> {
  const rows: Array<{ id: string; label: string; status: string }> = [];
  const seen = new Set<string>();
  const push = (id: string, label: string, status: string) => {
    if (seen.has(id)) {
      const i = rows.findIndex((r) => r.id === id);
      if (i >= 0) rows[i] = { id, label: label || rows[i].label, status };
      return;
    }
    seen.add(id);
    rows.push({ id, label, status });
  };
  for (const e of events) {
    const t = (e.type || "").toLowerCase();
    if (t === "subagent" || t === "task" || t === "agent") {
      const id = strField(e, "subagent_id", "agent_id", "task_id", "id") || `sub-${rows.length}`;
      push(id, strField(e, "name", "label", "description") || "子代理", strField(e, "status") || "running");
    }
    for (const c of e.tool_calls || []) {
      if (!isTaskTool(c.function?.name || "")) continue;
      const a = parseJsonObj(c.function?.arguments || "") || {};
      const id =
        (typeof a.id === "string" && a.id) ||
        (typeof a.subagent_id === "string" && a.subagent_id) ||
        c.id ||
        `task-${rows.length}`;
      const label =
        (typeof a.description === "string" && a.description) ||
        (typeof a.subagent_type === "string" && a.subagent_type) ||
        "Task";
      push(id, label, "running");
    }
    if (e.type === "tool" && isTaskTool(e.name || "")) {
      const id = e.tool_call_id || strField(e, "subagent_id") || `task-${rows.length}`;
      push(id, e.name || "Task", "done");
    }
  }
  return rows;
}

function todoClass(status?: string): string {
  const s = (status || "").toLowerCase().replace(/_/g, "");
  if (s === "completed" || s === "done" || s === "cancelled" || s === "canceled") return "done";
  if (s === "inprogress" || s === "running") return "run";
  return "";
}

function TodoBoard({ items }: { items: TodoItem[] }) {
  if (!items.length) return null;
  return (
    <div className="todo-board">
      <div className="todo-head">TodoWrite</div>
      <ul>
        {items.map((it, i) => (
          <li key={it.id || `${it.content}-${i}`} className={todoClass(it.status)}>
            {it.content}
          </li>
        ))}
      </ul>
    </div>
  );
}

function livePromptTokens(events: SessionEvent[], snap?: Snap): number {
  let last = 0;
  for (const e of events) {
    if (e.type === "assistant" && (e.prompt_tokens || 0) > 0) {
      last = e.prompt_tokens || 0;
    }
  }
  if (last > 0) return last;
  return usageLivePrompt(snap?.usage);
}

function compactCount(events: SessionEvent[], snap?: Snap): number {
  const n = events.filter((e) => e.type === "session/compact").length;
  return n || usageCompacts(snap?.usage);
}

export type RunPhase = "idle" | "waiting" | "thinking" | "writing" | "tool" | "permit" | "clarify" | "stopping";

export function runPhase(opts: {
  busy: boolean;
  aborting?: boolean;
  live: { think: string; content: string };
  events: SessionEvent[];
  permit?: Permit;
  clarify?: Clarify;
}): RunPhase {
  if (opts.clarify) return "clarify";
  if (opts.permit) return "permit";
  if (opts.aborting) return "stopping";
  const liveOn = !!(opts.live.think || opts.live.content);
  if (!opts.busy && !liveOn) return "idle";
  if (opts.live.content) return "writing";
  if (opts.live.think) return "thinking";
  const last = opts.events[opts.events.length - 1];
  if (last?.type === "tool") return "tool";
  if (last?.type === "assistant" && (last.tool_calls?.length ?? 0) > 0) return "tool";
  if (opts.busy) return "waiting";
  return "idle";
}

export const PHASE_LABEL: Record<RunPhase, string> = {
  idle: "空闲",
  waiting: "等待模型",
  thinking: "思考中",
  writing: "生成中",
  tool: "调用工具",
  permit: "等待审批",
  clarify: "AskQuestion",
  stopping: "正在停止",
};

export function fmtElapsed(s: number) {
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const r = s % 60;
  return `${m}:${r.toString().padStart(2, "0")}`;
}

export function RunChip({
  phase,
  elapsed,
  queued,
  steered,
  onClick,
}: {
  phase: RunPhase;
  elapsed: number;
  queued: number;
  steered: number;
  onClick?: () => void;
}) {
  const running = phase !== "idle";
  const bits = [PHASE_LABEL[phase]];
  if (running && elapsed > 0) bits.push(fmtElapsed(elapsed));
  if (queued > 0) bits.push(`排队 ${queued}`);
  if (steered > 0) bits.push(`转向 ${steered}`);
  return (
    <button
      type="button"
      className={`run-chip${running ? " on" : ""} phase-${phase}`}
      onClick={onClick}
      aria-label={`模型状态 ${bits.join(" · ")}`}
    >
      <span className="run-dot" />
      <span>{bits.join(" · ")}</span>
    </button>
  );
}

type PlusId = "imagine" | "attach" | "slash" | "approvals" | "scope" | "plan" | "clarify";

function ComposerPlus({
  open,
  onToggle,
  onClose,
  approvals,
  agentScope,
  planOn,
  clarifyOn,
  imagineOn,
  onAttach,
  onSlash,
  onCycleApprovals,
  onToggleScope,
  onTogglePlan,
  onToggleClarify,
  onToggleImagine,
}: {
  open: boolean;
  onToggle: () => void;
  onClose: () => void;
  approvals: string;
  agentScope: string;
  planOn: boolean;
  clarifyOn: boolean;
  imagineOn: boolean;
  onAttach: () => void;
  onSlash: () => void;
  onCycleApprovals: () => void;
  onToggleScope: () => void;
  onTogglePlan: () => void;
  onToggleClarify: () => void;
  onToggleImagine: () => void;
}) {
  const rootRef = useRef<HTMLDivElement>(null);
  const [sel, setSel] = useState(0);
  const menuId = "composer-plus-menu";
  const global = agentScope === "global";
  const items: Array<{
    id: PlusId;
    group: string;
    groupLabel: string;
    icon: string;
    label: string;
    hint: string;
    tone?: string;
    checked?: boolean;
    run: () => void;
  }> = [
    {
      id: "imagine",
      group: "add",
      groupLabel: "添加",
      icon: "image",
      label: "生成图片",
      hint: imagineOn ? "开 · 生图端点" : "生图端点",
      checked: imagineOn,
      run: onToggleImagine,
    },
    {
      id: "attach",
      group: "add",
      groupLabel: "添加",
      icon: "clip",
      label: "添加附件",
      hint: "拖入 / 粘贴",
      run: () => {
        onClose();
        onAttach();
      },
    },
    {
      id: "slash",
      group: "add",
      groupLabel: "添加",
      icon: "command",
      label: "斜杠命令",
      hint: "/",
      run: () => {
        onClose();
        onSlash();
      },
    },
    {
      id: "approvals",
      group: "session",
      groupLabel: "会话",
      icon: "lock",
      label: "审批",
      hint: approvals,
      tone: approvals === "yolo" ? "danger" : approvals === "auto" ? "warn" : undefined,
      run: onCycleApprovals,
    },
    {
      id: "scope",
      group: "session",
      groupLabel: "会话",
      icon: "folder",
      label: "工作区范围",
      hint: global ? "全局" : "工作区",
      tone: global ? "danger" : undefined,
      run: () => {
        onClose();
        onToggleScope();
      },
    },
    {
      id: "plan",
      group: "session",
      groupLabel: "会话",
      icon: "list",
      label: "计划模式",
      hint: planOn ? "开" : "关",
      checked: planOn,
      run: onTogglePlan,
    },
    {
      id: "clarify",
      group: "session",
      groupLabel: "会话",
      icon: "help",
      label: "AskQuestion",
      hint: clarifyOn ? "开" : "关",
      checked: clarifyOn,
      run: onToggleClarify,
    },
  ];
  const itemsRef = useRef(items);
  itemsRef.current = items;

  useEffect(() => {
    if (open) setSel(0);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open, onClose]);

  const onMenuKey = (e: ReactKey) => {
    if (!open) return;
    const list = itemsRef.current;
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setSel((i) => Math.min(list.length - 1, i + 1));
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      setSel((i) => Math.max(0, i - 1));
      return;
    }
    if (e.key === "Home") {
      e.preventDefault();
      setSel(0);
      return;
    }
    if (e.key === "End") {
      e.preventDefault();
      setSel(list.length - 1);
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      list[sel]?.run();
    }
  };

  const chips: Array<{ key: string; className: string; label: string; title: string; onClick: () => void }> = [];
  if (approvals !== "ask") {
    chips.push({
      key: "ap",
      className: `plus-chip ap-${approvals}`,
      label: approvals,
      title: `审批 ${approvals}，点击切换`,
      onClick: onCycleApprovals,
    });
  }
  if (global) {
    chips.push({
      key: "scope",
      className: "plus-chip scope-global",
      label: "全局",
      title: "Agent 可访问工作区外路径，点击收回工作区",
      onClick: onToggleScope,
    });
  }
  if (imagineOn) {
    chips.push({
      key: "imagine",
      className: "plus-chip on",
      label: "生图",
      title: "图片生成开启中，发送将调用生图端点，点击关闭",
      onClick: onToggleImagine,
    });
  }
  if (planOn) {
    chips.push({
      key: "plan",
      className: "plus-chip on",
      label: "plan",
      title: "计划模式开启中，点击关闭",
      onClick: onTogglePlan,
    });
  }
  if (clarifyOn) {
    chips.push({
      key: "clarify",
      className: "plus-chip on",
      label: "Ask",
      title: "AskQuestion 已开，点击关闭",
      onClick: onToggleClarify,
    });
  }

  return (
    <div className="plus-cluster" ref={rootRef} onKeyDown={onMenuKey}>
      <button
        type="button"
        className={`plus-btn${open ? " open" : ""}`}
        title="添加附件、生成图片与会话选项"
        aria-label="添加附件、生成图片与会话选项"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={menuId}
        onClick={onToggle}
      >
        <Icon name="plus" />
      </button>
      {open ? (
        <div className="plus-menu" id={menuId} role="menu" aria-label="添加与会话">
          {items.map((it, i) => {
            const prev = items[i - 1];
            const head = !prev || prev.group !== it.group;
            return (
              <div key={it.id}>
                {head ? <div className={`plus-menu-cap${i > 0 ? " sep" : ""}`}>{it.groupLabel}</div> : null}
                <button
                  type="button"
                  role="menuitem"
                  id={`plus-item-${it.id}`}
                  className={`plus-item${i === sel ? " sel" : ""}${it.tone ? ` tone-${it.tone}` : ""}`}
                  tabIndex={-1}
                  onMouseEnter={() => setSel(i)}
                  onClick={it.run}
                >
                  <Icon name={it.icon} />
                  <span className="plus-item-label">{it.label}</span>
                  <span className="plus-item-hint">{it.hint}</span>
                  {it.checked ? <Icon name="check" className="ico plus-check" /> : null}
                </button>
              </div>
            );
          })}
        </div>
      ) : null}
      {chips.length > 0 ? (
        <div className="plus-chips">
          {chips.map((c) => (
            <button key={c.key} type="button" className={c.className} title={c.title} onClick={c.onClick}>
              {c.label}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

export function ChatPage({
  snap,
  events,
  live,
  busy,
  permit,
  clarify,
  elapsed,
  detailsOpen,
  onToggleDetails,
  onReload,
}: {
  snap: Snap;
  events: SessionEvent[];
  live: { think: string; content: string };
  busy: boolean;
  permit: Permit;
  clarify: Clarify;
  elapsed: number;
  detailsOpen: boolean;
  onToggleDetails: () => void;
  onReload: () => Promise<void>;
}) {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [histOpen, setHistOpen] = useState(false);
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [text, setText] = useState("");
  const [atts, setAtts] = useState<Uploaded[]>([]);
  const [uploading, setUploading] = useState(0);
  const [slash, setSlash] = useState<typeof SLASH>([]);
  const [slashSel, setSlashSel] = useState(0);
  const [mentions, setMentions] = useState<FileHit[]>([]);
  const [mentionSel, setMentionSel] = useState(0);
  const [planReview, setPlanReview] = useState(false);
  const [aborting, setAborting] = useState(false);
  const [err, setErr] = useState("");
  const [plusOpen, setPlusOpen] = useState(false);
  const [dragOver, setDragOver] = useState(false);
  const [lightbox, setLightbox] = useState("");
  const [slashOut, setSlashOut] = useState("");
  const fileRef = useRef<HTMLInputElement>(null);
  const logRef = useRef<HTMLDivElement>(null);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const imeLock = useRef(false);
  const treeHold = useRef<FileHit[] | false | undefined>(undefined);
  const treePending = useRef<Promise<FileHit[] | null> | null>(null);
  const mentionQ = useRef("");
  const seenBusyInPlan = useRef(false);
  const wantPlanReview = useRef(false);
  const prevBusy = useRef(busy);
  const [showJump, setShowJump] = useState(false);
  const [openBlocks, setOpenBlocks] = useState<Set<string>>(() => new Set());
  const [openSteps, setOpenSteps] = useState<Set<string>>(() => new Set());
  const [previewPath, setPreviewPath] = useState("");
  const [previewMax, setPreviewMax] = useState(false);
  const [detailsTab, setDetailsTab] = useState<"preview" | "arts" | "session">("session");
  const [railWidth, setRailWidth] = useState(520);
  const lastArtRef = useRef("");
  const lastUpload = useRef({ sig: "", t: 0 });
  const dragRail = useRef<{ x: number; w: number } | null>(null);
  const anchorRef = useRef<{ count: number; sess?: string }>({ count: 0, sess: undefined });

  const turns = useMemo(() => buildTurns(events), [events]);
  const todos = useMemo(() => latestTodos(events), [events]);
  const agents = useMemo(() => subagentRows(events), [events]);
  const lastUserKey = useMemo(() => {
    for (let i = turns.length - 1; i >= 0; i--) {
      if (turns[i].user !== undefined) return turns[i].key;
    }
    return null;
  }, [turns]);
  const userTurnCount = useMemo(() => turns.filter((t) => t.user !== undefined).length, [turns]);

  const toggleBlock = useCallback(
    (k: string) =>
      setOpenBlocks((s) => {
        const n = new Set(s);
        if (n.has(k)) n.delete(k);
        else n.add(k);
        return n;
      }),
    [],
  );
  const toggleStep = useCallback(
    (k: string) =>
      setOpenSteps((s) => {
        const n = new Set(s);
        if (n.has(k)) n.delete(k);
        else n.add(k);
        return n;
      }),
    [],
  );

  const updateJump = () => {
    const el = logRef.current;
    if (!el) return;
    setShowJump(el.scrollHeight - el.scrollTop - el.clientHeight > 240);
  };

  const refreshSessions = async () => {
    try {
      const j = await rpc<{ sessions?: SessionInfo[] }>("session.list", {});
      setSessions(j.sessions || []);
    } catch {
      /* list is best-effort */
    }
  };

  useEffect(() => {
    refreshSessions();
  }, [snap.session]);

  useEffect(() => {
    if (histOpen) void refreshSessions();
    else setPicked(new Set());
  }, [histOpen]);

  useEffect(() => {
    setPicked((cur) => {
      const live = new Set(sessions.map((s) => s.id));
      const next = new Set([...cur].filter((id) => live.has(id)));
      return next.size === cur.size ? cur : next;
    });
  }, [sessions]);

  useEffect(() => {
    const el = logRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      el.style.setProperty("--thread-h", `${el.clientHeight}px`);
      updateJump();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const block = (e: DragEvent) => {
      if (e.dataTransfer?.types && [...e.dataTransfer.types].includes("Files")) e.preventDefault();
    };
    window.addEventListener("dragover", block);
    window.addEventListener("drop", block);
    return () => {
      window.removeEventListener("dragover", block);
      window.removeEventListener("drop", block);
    };
  }, []);

  useEffect(() => {
    updateJump();
  }, [events, live, busy]);

  useEffect(() => {
    setOpenBlocks(new Set());
    setOpenSteps(new Set());
    setSlashOut("");
    setText("");
    setAtts([]);
    setErr("");
    setSlash([]);
    setMentions([]);
    setPlanReview(false);
    setLightbox("");
    setDragOver(false);
    seenBusyInPlan.current = false;
    wantPlanReview.current = false;
    prevBusy.current = busy;
    treeHold.current = undefined;
    treePending.current = null;
  }, [snap.session]);

  /**
   * 新的用户消息锚到视口顶部，回复在下方生长；流式期间不追底。
   * 只在「用户轮数增加」或「切会话」时滚动——compact / undo 触发的
   * history.replace 会减少轮数，不应劫持滚动位置。
   */
  useEffect(() => {
    const el = logRef.current;
    if (!el) return;
    const sessChanged = anchorRef.current.sess !== snap.session;
    const grew = userTurnCount > anchorRef.current.count;
    anchorRef.current = { count: userTurnCount, sess: snap.session };
    if (!sessChanged && !grew) return;
    if (!lastUserKey) return;
    const target = el.querySelector<HTMLElement>(`[data-turn="${lastUserKey}"]`);
    if (!target) return;
    const top =
      target.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop - 10;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    el.scrollTo({ top: Math.max(0, top), behavior: sessChanged || reduce ? "auto" : "smooth" });
  }, [userTurnCount, lastUserKey, snap.session]);

  useEffect(() => {
    if (!busy) setAborting(false);
  }, [busy]);

  useEffect(() => {
    treeHold.current = undefined;
    treePending.current = null;
  }, [snap.workspace]);

  useEffect(() => {
    if (!snap.plan_mode) {
      seenBusyInPlan.current = false;
      wantPlanReview.current = false;
      prevBusy.current = busy;
      setPlanReview(false);
      return;
    }
    if (busy) {
      seenBusyInPlan.current = true;
      wantPlanReview.current = false;
      setPlanReview(false);
    } else if (prevBusy.current && seenBusyInPlan.current) {
      wantPlanReview.current = true;
    }
    prevBusy.current = busy;
    if (permit || clarify) return;
    if (wantPlanReview.current && hasRecentAssistant(events)) {
      wantPlanReview.current = false;
      setPlanReview(true);
    }
  }, [busy, snap.plan_mode, events, permit, clarify]);

  /** 命令输出追加在底部，滚过去让用户看到。 */
  useEffect(() => {
    if (!slashOut) return;
    const el = logRef.current;
    if (!el) return;
    requestAnimationFrame(() => {
      const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      el.scrollTo({ top: el.scrollHeight, behavior: reduce ? "auto" : "smooth" });
    });
  }, [slashOut]);

  /** 历史抽屉 Esc 关闭。 */
  useEffect(() => {
    if (!histOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setHistOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [histOpen]);

  /** 输入框随内容长高（上限交给 CSS max-height）。 */
  useEffect(() => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    ta.style.height = `${Math.min(180, ta.scrollHeight)}px`;
  }, [text]);

  const upload = async (files: FileList | File[]) => {
    const list = [...files];
    if (!list.length) return;
    const sig = list.map((f) => `${f.name}:${f.size}:${f.lastModified}`).join("|");
    const now = Date.now();
    if (sig === lastUpload.current.sig && now - lastUpload.current.t < 500) return;
    lastUpload.current = { sig, t: now };
    const fd = new FormData();
    list.forEach((f) => fd.append("file", f));
    setUploading((n) => n + 1);
    try {
      const r = await fetch("/api/upload", { method: "POST", body: fd });
      if (!r.ok) throw new Error(await r.text());
      const j = (await r.json()) as { files: Uploaded[] };
      setAtts((xs) => [...xs, ...(j.files || [])]);
      setErr("");
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setUploading((n) => Math.max(0, n - 1));
    }
  };

  const attachClipboard = async (data: DataTransfer | null) => {
    const plan = clipboardPaste(data);
    if (!plan.files.length && !plan.urls.length) return false;
    if (plan.insertText) {
      const ta = taRef.current;
      const start = ta?.selectionStart ?? text.length;
      const end = ta?.selectionEnd ?? start;
      const { next, caret } = insertAtCaret(text, plan.insertText, start, end);
      setText(next);
      requestAnimationFrame(() => {
        const el = taRef.current;
        if (!el) return;
        el.focus();
        el.setSelectionRange(caret, caret);
        refreshPickers(next, caret);
      });
    }
    const dataFiles = plan.urls
      .filter((u) => u.startsWith("data:"))
      .map((u) => fileFromDataUri(u, "clipboard.png"))
      .filter((f): f is File => !!f);
    const remotes = plan.urls.filter((u) => /^https?:\/\//i.test(u));
    if (plan.files.length || dataFiles.length) await upload([...plan.files, ...dataFiles]);
    if (remotes.length) {
      setUploading((n) => n + 1);
      try {
        const extra = await ingestRemoteImages(remotes);
        if (extra.length) {
          setAtts((xs) => [...xs, ...extra]);
          setErr("");
        } else if (!plan.files.length && !dataFiles.length) {
          setErr("无法从剪贴板获取图片");
        }
      } catch (e) {
        setErr(failMsg(e));
      } finally {
        setUploading((n) => Math.max(0, n - 1));
      }
    }
    return true;
  };

  const applySlash = (cmd: string) => {
    setText(cmd + " ");
    setSlash([]);
    setMentions([]);
    taRef.current?.focus();
  };

  const applyMention = (hit: FileHit) => {
    const ta = taRef.current;
    const cursor = ta?.selectionStart ?? text.length;
    const m = mentionToken(text, cursor);
    if (!m) {
      setMentions([]);
      return;
    }
    const insert = `@${hit.path}`;
    const needsSpace = m.end >= text.length || !/\s/.test(text[m.end]);
    const next = text.slice(0, m.start) + insert + (needsSpace ? " " : "") + text.slice(m.end);
    const caret = m.start + insert.length + (needsSpace ? 1 : 0);
    setText(next);
    setMentions([]);
    requestAnimationFrame(() => {
      taRef.current?.focus();
      taRef.current?.setSelectionRange(caret, caret);
    });
  };

  const ensureTree = async (): Promise<FileHit[] | null> => {
    if (treeHold.current === false) return null;
    if (Array.isArray(treeHold.current)) return treeHold.current;
    if (!treePending.current) {
      treePending.current = api<{ ok?: boolean; entries?: unknown[] }>("/tree")
        .then((j) => {
          if (j && j.ok === false) {
            treeHold.current = false;
            return null;
          }
          const rows = Array.isArray(j.entries) ? j.entries : [];
          const hits = rows.map(parseTreeEntry).filter((x): x is FileHit => !!x);
          treeHold.current = hits;
          return hits;
        })
        .catch(() => {
          treeHold.current = false;
          return null;
        })
        .finally(() => {
          treePending.current = null;
        });
    }
    return treePending.current;
  };

  const refreshPickers = (value: string, cursor: number) => {
    if (value.startsWith("/")) {
      mentionQ.current = "";
      const q = value.slice(1).toLowerCase();
      const rows = SLASH.filter(([c, d]) => (c + d).toLowerCase().includes(q));
      setSlash(rows);
      setSlashSel(0);
      setMentions([]);
      return;
    }
    setSlash([]);
    const m = mentionToken(value, cursor);
    if (!m) {
      mentionQ.current = "";
      setMentions([]);
      return;
    }
    void ensureTree().then((hits) => {
      if (!hits) {
        setMentions([]);
        return;
      }
      const ta = taRef.current;
      const now = mentionToken(ta?.value ?? value, ta?.selectionStart ?? cursor);
      if (!now) {
        mentionQ.current = "";
        setMentions([]);
        return;
      }
      const rows = fuzzyFiles(hits, now.query);
      setMentions(rows);
      if (mentionQ.current !== now.query) {
        mentionQ.current = now.query;
        setMentionSel(0);
      } else {
        setMentionSel((s) => (rows.length ? Math.min(s, rows.length - 1) : 0));
      }
    });
  };

  const togglePick = (id: string, on: boolean) => {
    setPicked((cur) => {
      const next = new Set(cur);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  };

  const deleteSessions = async (ids: string[]) => {
    if (ids.length === 0) return;
    if (busy && snap.session && ids.includes(snap.session)) {
      setErr("当前会话正在回复，先停止再删。");
      return;
    }
    const one = ids.length === 1 ? sessions.find((s) => s.id === ids[0]) : undefined;
    const label =
      ids.length === 1 ? `删除会话「${one ? sessionName(one) : ids[0]}」？` : `删除 ${ids.length} 个会话？`;
    if (!(await uiConfirm(label, "会话记录与标题将一并删除，无法恢复。", { danger: true, okLabel: "删除" }))) return;
    setErr("");
    try {
      await rpc("session.delete", ids.length === 1 ? { session: ids[0] } : { sessions: ids });
      setPicked(new Set());
      await refreshSessions();
      await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const send = async () => {
    const raw = text.trim();
    if (!raw && !atts.length) return;
    setErr("");
    if (raw.startsWith("/")) {
      try {
        const j = await rpc<{ text?: string }>("slash", { text: raw });
        setText("");
        setSlash([]);
        setMentions([]);
        setSlashOut((j.text || "").trim());
        await onReload();
        await refreshSessions();
      } catch (e) {
        setErr(failMsg(e));
      }
      return;
    }
    const { prompt, content_parts } = attachPayload(raw, atts);
    if (snap.imagine_mode && !raw) {
      setErr("生图需要文字描述");
      return;
    }
    try {
      await rpc("turn.start", { prompt, content_parts });
      setText("");
      setAtts([]);
      setSlash([]);
      setMentions([]);
      setSlashOut("");
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const stop = async () => {
    setAborting(true);
    setErr("");
    try {
      await rpc("turn.abort", {});
    } catch (e) {
      setErr(failMsg(e));
      setAborting(false);
    }
  };

  const steer = async () => {
    const raw = text.trim();
    if (!raw && !atts.length) return;
    setErr("");
    const { prompt, content_parts } = attachPayload(raw, atts);
    try {
      await rpc("turn.steer", { text: prompt, content_parts });
      setText("");
      setAtts([]);
      setMentions([]);
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const queue = async () => {
    const raw = text.trim();
    if (!raw && !atts.length) return;
    setErr("");
    const { prompt, content_parts } = attachPayload(raw, atts);
    try {
      await rpc("turn.queue", { prompt, content_parts });
      setText("");
      setAtts([]);
      setMentions([]);
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const arts = useMemo(() => turnArtifacts(events, snap.workspace), [events, snap.workspace]);
  const previewList = useMemo(() => turnPreviewPaths(events, snap.workspace), [events, snap.workspace]);
  const liveUser = useMemo(() => lastLiveUserIndex(events), [events]);
  const openPreview = useCallback((p: string) => {
    if (!p) return;
    setPreviewPath(p);
    setDetailsTab("preview");
    if (!detailsOpen) onToggleDetails();
  }, [detailsOpen, onToggleDetails]);
  useEffect(() => {
    lastArtRef.current = "";
    setPreviewPath("");
    setPreviewMax(false);
  }, [liveUser, snap.session]);
  useEffect(() => {
    let gone = false;
    (async () => {
      for (const p of previewList) {
        if (gone) return;
        if (isOfficeKind(kindFor(p).id)) {
          try {
            const r = await fetch(`/api/office/config?path=${encodeURIComponent(p)}`);
            if (!r.ok) continue;
          } catch {
            continue;
          }
        }
        if (p === lastArtRef.current) return;
        lastArtRef.current = p;
        openPreview(p);
        return;
      }
    })();
    return () => {
      gone = true;
    };
  }, [previewList]);
  useEffect(() => {
    const onMove = (e: MouseEvent) => {
      const drag = dragRail.current;
      if (!drag) return;
      const next = Math.min(720, Math.max(280, drag.w + (drag.x - e.clientX)));
      setRailWidth(next);
    };
    const onUp = () => {
      dragRail.current = null;
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, []);
  const usage = snap.usage;
  const win = snap.window || 0;
  const used = livePromptTokens(events, snap);
  const rawPct = win ? Math.round((used / win) * 100) : 0;
  const pct = Math.min(100, Math.max(0, rawPct));
  const compacts = compactCount(events, snap);
  const hitPct = usageHitPct(usage);
  const hit = usageCachedReported(usage) && hitPct != null ? `${hitPct.toFixed(1)}%` : "n/a";
  const queued = snap.queued ?? 0;
  const steered = snap.steered ?? 0;
  const policy = snap.busy || "interrupt";
  const phase = runPhase({ busy, aborting, live, events, permit, clarify });
  const waitPrefix =
    phase === "waiting" ? usageLivePrompt(usage) : 0;
  const waitPrefixBit = waitPrefix > 0 ? ` · ${waitPrefix} tokens` : "";
  const callLabel =
    phase === "stopping" || phase === "permit" || phase === "clarify"
      ? PHASE_LABEL[phase]
      : snap.imagine_mode && (phase === "waiting" || phase === "writing")
        ? "正在生成图片"
        : waitPrefix > 0
          ? `正在调用模型 · ${waitPrefix.toLocaleString()} tokens`
          : "正在调用模型";

  /** 把流式中的思考/正文合并进最后一轮：思考进轨迹块，正文在下方流式生长。 */
  const withLive = (blocks: Block[]): Block[] => {
    let out = blocks;
    if (live.think) {
      const last = out[out.length - 1];
      if (last && last.kind === "activity") {
        out = [
          ...out.slice(0, -1),
          { kind: "activity", steps: pushThink(last.steps, live.think, !live.content) },
        ];
      } else {
        out = [...out, { kind: "activity", steps: [{ kind: "think", text: live.think, live: !live.content }] }];
      }
    }
    if (live.content) {
      out = [...out, { kind: "text", text: live.content, live: true }];
    } else if (busy && !live.think) {
      const last = out[out.length - 1];
      if (!last || last.kind !== "activity") out = [...out, { kind: "activity", steps: [] }];
    }
    return out;
  };
  const draft = text.trim();
  const heading = nameFromEvents(events, snap.title);
  const approvals = snap.approvals || "ask";
  const agentScope = snap.agent_scope || "workspace";

  const newChat = async () => {
    setErr("");
    try {
      await rpc("session.new", {});
      await refreshSessions();
      await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  /** 审批模式就地轮换（ask → auto → yolo）。走 /api/config，会同步 PermitHub 并写盘。 */
  const cycleApprovals = async () => {
    const order = ["ask", "auto", "yolo"];
    const next = order[(order.indexOf(approvals) + 1) % order.length];
    setErr("");
    try {
      await api("/config", { method: "POST", body: JSON.stringify({ approvals: next }) });
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const toggleAgentScope = async () => {
    const toGlobal = agentScope !== "global";
    if (toGlobal) {
      const ok = await uiConfirm(
        "切换到全局作用域？",
        "文件工具将可以访问工作区以外的绝对路径。终端与 Python 仍从工作区启动，但不是系统沙箱。",
        { danger: true, okLabel: "切换到全局" },
      );
      if (!ok) return;
    }
    setErr("");
    try {
      const saved = await api<{ agent_scope?: string }>("/config", {
        method: "POST",
        body: JSON.stringify({ workspace_write_only: !toGlobal }),
      });
      const expected = toGlobal ? "global" : "workspace";
      if (saved.agent_scope !== expected) {
        throw new Error("作用域未被后端应用，请重启 grok-hyper 服务后重试");
      }
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const toggleClarify = async () => {
    setErr("");
    try {
      await rpc("slash", { text: snap.clarify_mode ? "/clarify off" : "/clarify on" });
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const toggleImagine = async () => {
    const turningOn = !snap.imagine_mode;
    setErr("");
    setPlusOpen(false);
    try {
      await rpc("slash", { text: turningOn ? "/imagine on" : "/imagine off" });
      const raw = text.trim();
      if (turningOn && !busy && raw && !raw.startsWith("/")) {
        const { prompt, content_parts } = attachPayload(raw, atts);
        await rpc("turn.start", { prompt, content_parts });
        setText("");
        setAtts([]);
        setSlash([]);
        setMentions([]);
        setSlashOut("");
        return;
      }
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const togglePlan = async () => {
    setErr("");
    try {
      await rpc("slash", { text: snap.plan_mode ? "/plan off" : "/plan on" });
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const dismissPlanReview = () => setPlanReview(false);

  const planImplement = async () => {
    setPlanReview(false);
    setErr("");
    try {
      await rpc("slash", { text: "/plan go" });
      await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const planExit = async () => {
    setPlanReview(false);
    setErr("");
    try {
      await rpc("slash", { text: "/plan off" });
      await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const sendLabel = !busy
    ? snap.imagine_mode
      ? "生成"
      : "发送"
    : policy === "queue"
      ? "排队"
      : policy === "steer"
        ? "转向"
        : "打断";
  const placeholder = snap.imagine_mode && !busy
    ? "描述要生成的图片… Enter 调用生图端点"
    : !busy
    ? "给 grok-hyper 发消息…  / 唤起命令，粘贴图片走上传"
    : policy === "queue"
      ? "本轮结束后会跑这段话…"
      : policy === "steer"
        ? "下一次工具结果后注入这段引导…"
        : "发送将打断当前轮次并改跑这段话…";
  const policyHint =
    policy === "queue"
      ? "忙碌策略 queue：Enter 排到本轮之后"
      : policy === "steer"
        ? "忙碌策略 steer：Enter 在下一次工具后注入"
        : "忙碌策略 interrupt：Enter 打断本轮";

  return (
    <div
      className={`page chat-page${previewMax ? " pv-maxed" : ""}`}
      onClick={(e) => {
        const t = e.target;
        if (!(t instanceof HTMLElement)) return;
        const img = t.closest("img.md-img, img.att-shot, img.a-thumb, img.preview-img");
        if (img instanceof HTMLImageElement && img.src) {
          e.preventDefault();
          setLightbox(img.currentSrc || img.src);
        }
      }}
      onPaste={(e) => {
        const t = e.target;
        if (t instanceof HTMLElement) {
          if (t.closest(".overlay")) return;
          if ((t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement) && t !== taRef.current) {
            return;
          }
        }
        const plan = clipboardPaste(e.clipboardData);
        if (!plan.files.length && !plan.urls.length) return;
        e.preventDefault();
        void attachClipboard(e.clipboardData);
      }}
    >
      <div className="chat-col">
        <div className="chat-top">
          <div className="chat-who">
            <strong className="ellipsis">{heading}</strong>
            {snap.session ? <span className="sid">{snap.session}</span> : null}
          </div>
          <span className="chip mono hide-narrow" title="会话模式，/mode 切换">{snap.mode || "agent"}</span>
          <span className="spacer" />
          <button className="btn ghost small" title="浏览与恢复历史会话" onClick={() => setHistOpen(true)}>
            <Icon name="clock" />
            历史
          </button>
          <button className="btn primary small" onClick={newChat}>
            <Icon name="plus" />
            新建聊天
          </button>
          <button
            type="button"
            className={`icon-btn${detailsOpen ? " on" : ""}`}
            title={detailsOpen ? "收起会话状态面板" : "展开会话状态面板"}
            aria-label="会话状态面板"
            aria-pressed={detailsOpen}
            onClick={onToggleDetails}
          >
            <Icon name="panel" />
          </button>
        </div>
        <div className="thread" ref={logRef} onScroll={updateJump}>
          <div className="thread-inner">
            {turns.length === 0 && !busy && !live.content && !live.think ? (
              <Empty title="开始对话" body="在下方输入。Enter 发送。需要选择题时打开 AskQuestion。文稿和下载在「文件」页。" />
            ) : null}
            {todos.length ? <TodoBoard items={todos} /> : null}
            {agents.map((a) => (
              <div className="subagent-row" key={a.id}>
                <span className={`st-dot ${a.status === "done" || a.status === "completed" ? "ok" : "run"}`} />
                <b>Task</b>
                <span className="grow ellipsis">{a.label}</span>
                <span className="mono">{a.id}</span>
                <span className="pill idle">{a.status}</span>
              </div>
            ))}
            {turns.map((t, ti) => {
              const isLast = ti === turns.length - 1;
              return (
                <TurnView
                  key={t.key}
                  turnKey={t.key}
                  user={t.user}
                  userMedia={t.userMedia}
                  blocks={isLast ? withLive(t.blocks) : t.blocks}
                  active={busy && isLast}
                  decodeTokS={busy && isLast ? null : t.decodeTokS}
                  callLabel={isLast ? callLabel : ""}
                  elapsed={isLast ? elapsed : 0}
                  openBlocks={openBlocks}
                  openSteps={openSteps}
                  onToggleBlock={toggleBlock}
                  onToggleStep={toggleStep}
                  onOpenPreview={openPreview}
                />
              );
            })}
            {slashOut ? (
              <div className="msg system">
                <div className="meta">
                  <span>命令</span>
                </div>
                <div className="bubble">{slashOut}</div>
              </div>
            ) : null}
          </div>
        </div>
        <div className="jump-anchor">
          <button
            type="button"
            className={`jump-bottom${showJump ? " on" : ""}`}
            aria-label="滚到最新"
            title="滚到最新"
            onClick={() => {
              const el = logRef.current;
              if (!el) return;
              const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
              el.scrollTo({ top: el.scrollHeight, behavior: reduce ? "auto" : "smooth" });
            }}
          >
            <Icon name="chev-d" />
          </button>
        </div>
        <div
          className={`composer-wrap${dragOver ? " drop" : ""}`}
          onDragEnter={(e) => {
            e.preventDefault();
            if (e.dataTransfer.types && [...e.dataTransfer.types].includes("Files")) setDragOver(true);
          }}
          onDragOver={(e) => e.preventDefault()}
          onDragLeave={(e) => {
            const next = e.relatedTarget;
            if (next instanceof Node && e.currentTarget.contains(next)) return;
            setDragOver(false);
          }}
          onDrop={(e) => {
            e.preventDefault();
            e.stopPropagation();
            setDragOver(false);
            if (e.dataTransfer.files.length) void upload(e.dataTransfer.files);
          }}
        >
          {dragOver ? <div className="drop-hint">放到这里，发给 grok-hyper</div> : null}
          <div className="composer-inner">
            {err ? <div className="err" style={{ margin: "0 0 8px" }}>{err}</div> : null}
            {busy ? (
              <div className="runbar" role="status">
                <span className={`run-dot lg phase-${phase}`} />
                <div className="run-copy">
                  <b>{PHASE_LABEL[phase]}</b>
                  <span>
                    {snap.model || "model"}
                    {elapsed > 0 ? ` · ${fmtElapsed(elapsed)}` : ""}
                    {waitPrefixBit}
                    {policy !== "interrupt" ? ` · busy ${policy}` : ""}
                  </span>
                </div>
                {queued > 0 ? (
                  <span className="run-pill on" title={(snap.queue_preview || []).join("\n")}>
                    排队 {queued}
                  </span>
                ) : (
                  <span className="run-pill idle">排队 0</span>
                )}
                {steered > 0 ? (
                  <span className="run-pill on" title={(snap.steer_preview || []).join("\n")}>
                    转向 {steered}
                  </span>
                ) : (
                  <span className="run-pill idle">转向 0</span>
                )}
                <div className="run-actions">
                  <button type="button" className="btn danger small" onClick={stop} disabled={aborting} title="中止本轮">
                    <Icon name="stop" />
                    停止
                  </button>
                  <button
                    type="button"
                    className="btn ghost small"
                    onClick={steer}
                    disabled={(!draft && atts.length === 0) || aborting}
                    title="下一次工具结果后注入输入框内容"
                  >
                    <Icon name="steer" />
                    转向
                  </button>
                  <button
                    type="button"
                    className="btn ghost small"
                    onClick={queue}
                    disabled={(!draft && atts.length === 0) || aborting}
                    title="本轮结束后再跑输入框内容"
                  >
                    <Icon name="queue" />
                    排队
                  </button>
                </div>
              </div>
            ) : null}
            {slash.length > 0 ? (
              <div className="slash-pop" role="listbox">
                {slash.map(([c, d], i) => (
                  <button
                    key={c}
                    type="button"
                    className={`slash-item${i === slashSel ? " sel" : ""}`}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      applySlash(c);
                    }}
                  >
                    <span className="cmd">{c}</span>
                    <span className="desc">{d}</span>
                  </button>
                ))}
              </div>
            ) : mentions.length > 0 ? (
              <div className="slash-pop" role="listbox">
                {mentions.map((hit, i) => (
                  <button
                    key={`${hit.path}:${i}`}
                    type="button"
                    className={`slash-item${i === mentionSel ? " sel" : ""}`}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      applyMention(hit);
                    }}
                  >
                    <span className="cmd">{hit.dir ? `${hit.name}/` : hit.name}</span>
                    <span className="desc">{hit.path}</span>
                  </button>
                ))}
              </div>
            ) : null}
            <div className="composer">
              {atts.length > 0 ? (
                <div className="att-row">
                  {atts.map((f, i) => (
                    <div
                      className={`att-card${f.kind === "image" || f.kind === "video" ? " media" : ""}`}
                      key={f.path}
                      role="button"
                      tabIndex={0}
                      title={`预览 ${f.name}`}
                      onClick={() => openPreview(f.path)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          openPreview(f.path);
                        }
                      }}
                    >
                      {f.kind === "image" ? (
                        <img className="att-shot" src={f.url} alt={f.name} />
                      ) : f.kind === "video" ? (
                        <video className="att-shot" src={f.url} muted playsInline />
                      ) : (
                        <span className="att-file">
                          <Icon name="file" />
                          {f.name}
                        </span>
                      )}
                      {f.kind === "image" || f.kind === "video" ? <span className="att-name">{f.name}</span> : null}
                      <button
                        type="button"
                        className="att-x"
                        aria-label={`移除 ${f.name}`}
                        onClick={(e) => {
                          e.stopPropagation();
                          setAtts(atts.filter((_, j) => j !== i));
                        }}
                      >
                        ×
                      </button>
                    </div>
                  ))}
                  {busy ? <span className="sub">附件会随排队/转向一起送出</span> : null}
                </div>
              ) : null}
              <textarea
                ref={taRef}
                rows={1}
                value={text}
                placeholder={placeholder}
                onChange={(e) => {
                  const v = e.target.value;
                  setPlusOpen(false);
                  setText(v);
                  refreshPickers(v, e.target.selectionStart ?? v.length);
                }}
                onFocus={() => setPlusOpen(false)}
                onSelect={(e) => {
                  const el = e.currentTarget;
                  if (!el.value.startsWith("/")) refreshPickers(el.value, el.selectionStart ?? el.value.length);
                }}
                onKeyDown={(e) => {
                  if (imeBusy(e) || imeLock.current) return;
                  if (slash.length && e.key === "Escape") {
                    e.preventDefault();
                    setSlash([]);
                    return;
                  }
                  if (mentions.length && e.key === "Escape") {
                    e.preventDefault();
                    setMentions([]);
                    return;
                  }
                  if (slash.length && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
                    e.preventDefault();
                    setSlashSel((s) =>
                      e.key === "ArrowDown" ? Math.min(slash.length - 1, s + 1) : Math.max(0, s - 1),
                    );
                    return;
                  }
                  if (mentions.length && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
                    e.preventDefault();
                    setMentionSel((s) =>
                      e.key === "ArrowDown" ? Math.min(mentions.length - 1, s + 1) : Math.max(0, s - 1),
                    );
                    return;
                  }
                  if (slash.length && e.key === "Tab") {
                    e.preventDefault();
                    applySlash(slash[slashSel][0]);
                    return;
                  }
                  if (mentions.length && e.key === "Tab") {
                    e.preventDefault();
                    applyMention(mentions[mentionSel]);
                    return;
                  }
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    if (slash.length) {
                      // 敲全命令（含带参数）直接回车执行；半截命令先补全。
                      const cmd = slash[slashSel][0];
                      const typed = text.trim();
                      if (typed === cmd || typed.startsWith(`${cmd} `)) {
                        setSlash([]);
                        send();
                      } else applySlash(cmd);
                    } else if (mentions.length) {
                      applyMention(mentions[mentionSel]);
                    } else send();
                  }
                }}
                onCompositionStart={() => {
                  imeLock.current = true;
                }}
                onCompositionEnd={() => {
                  // Some engines fire compositionend, then a leftover Enter in the same frame.
                  imeLock.current = true;
                  requestAnimationFrame(() => {
                    imeLock.current = false;
                  });
                }}
                onPaste={(e) => {
                  const plan = clipboardPaste(e.clipboardData);
                  if (!plan.files.length && !plan.urls.length) return;
                  e.preventDefault();
                  e.stopPropagation();
                  void attachClipboard(e.clipboardData);
                }}
                onDragOver={(e) => e.preventDefault()}
              />
              <div className="composer-bar">
                <input
                  ref={fileRef}
                  type="file"
                  multiple
                  hidden
                  onChange={(e) => {
                    if (e.target.files) void upload(e.target.files);
                    e.target.value = "";
                  }}
                />
                <ComposerPlus
                  open={plusOpen}
                  onToggle={() => setPlusOpen((v) => !v)}
                  onClose={() => setPlusOpen(false)}
                  approvals={approvals}
                  agentScope={agentScope}
                  planOn={!!snap.plan_mode}
                  clarifyOn={!!snap.clarify_mode}
                  imagineOn={!!snap.imagine_mode}
                  onAttach={() => fileRef.current?.click()}
                  onSlash={() => {
                    setText("/");
                    setSlash(SLASH);
                    setSlashSel(0);
                    setMentions([]);
                    taRef.current?.focus();
                  }}
                  onCycleApprovals={() => void cycleApprovals()}
                  onToggleScope={() => void toggleAgentScope()}
                  onTogglePlan={() => void togglePlan()}
                  onToggleClarify={() => void toggleClarify()}
                  onToggleImagine={() => void toggleImagine()}
                />
                {uploading > 0 ? (
                  <span className="upl-chip">
                    <span className="act-spin" aria-hidden />
                    上传中…
                  </span>
                ) : null}
                <span className="spacer" style={{ flex: 1 }} />
                <span className="composer-hint">
                  {busy
                    ? `${policyHint} · 也可点停止 / 转向 / 排队`
                    : snap.imagine_mode
                      ? "Enter 生成图片 · Shift+Enter 换行"
                      : "Enter 发送 · Shift+Enter 换行"}
                </span>
                <button
                  type="button"
                  className="send-btn"
                  onClick={send}
                  disabled={uploading > 0 || (!draft && atts.length === 0) || (!!snap.imagine_mode && !draft)}
                  title={uploading > 0 ? "附件上传中…" : undefined}
                >
                  {sendLabel} <Icon name="arrow-up" />
                </button>
              </div>
            </div>
          </div>
        </div>
        {histOpen ? (
          <>
            <div className="drawer-mask" onClick={() => setHistOpen(false)} />
            <aside className="drawer" aria-label="聊天历史">
              <header>
                <label className="tick" title="全选">
                  <input
                    type="checkbox"
                    checked={sessions.length > 0 && sessions.every((s) => picked.has(s.id))}
                    disabled={sessions.length === 0}
                    onChange={(e) => setPicked(e.target.checked ? new Set(sessions.map((s) => s.id)) : new Set())}
                    aria-label="全选会话"
                  />
                </label>
                <b>聊天历史</b>
                <span className="spacer" style={{ flex: 1 }} />
                {picked.size > 0 ? (
                  <button type="button" className="btn danger small" onClick={() => void deleteSessions([...picked])}>
                    删除 {picked.size}
                  </button>
                ) : null}
                <button className="btn ghost small" onClick={() => setHistOpen(false)}>
                  关闭
                </button>
              </header>
              <div style={{ overflow: "auto", flex: 1 }}>
                {sessions.length === 0 ? <Empty title="没有会话" body="点新建聊天开始。" /> : null}
                {sessions.map((s) => (
                  <div key={s.id} className={`session-row${s.id === snap.session ? " on" : ""}`}>
                    <label className="tick">
                      <input
                        type="checkbox"
                        checked={picked.has(s.id)}
                        onChange={(e) => togglePick(s.id, e.target.checked)}
                        aria-label={`选择 ${sessionName(s)}`}
                      />
                    </label>
                    <button
                      type="button"
                      className="session-item"
                      onClick={async () => {
                        setErr("");
                        try {
                          await rpc("session.resume", { session: s.id });
                          setHistOpen(false);
                          await refreshSessions();
                          await onReload();
                        } catch (e) {
                          setErr(failMsg(e));
                        }
                      }}
                    >
                      <div className="t">{sessionName(s)}</div>
                      {s.id ? <div className="sid">{s.id}</div> : null}
                      <div className="m">
                        {s.mode || "agent"} · {s.channel || "console"} · {s.events ?? 0} events
                      </div>
                    </button>
                    <button
                      type="button"
                      className="btn ghost small session-del"
                      title="删除"
                      aria-label={`删除 ${sessionName(s)}`}
                      onClick={() => void deleteSessions([s.id])}
                    >
                      <Icon name="trash" />
                    </button>
                  </div>
                ))}
              </div>
            </aside>
          </>
        ) : null}
      </div>
      <aside
        className={`details${detailsOpen ? "" : " closed"}${previewPath && detailsTab === "preview" ? " wide" : ""}${previewMax ? " pv-fill" : ""}`}
        style={detailsOpen && previewPath && detailsTab === "preview" && !previewMax ? { width: railWidth, flex: "0 0 auto" } : undefined}
      >
        <div
          className="dt-split"
          onMouseDown={(e) => {
            dragRail.current = { x: e.clientX, w: railWidth };
            e.preventDefault();
          }}
          aria-hidden
        />
        <div className="dt-head">
          结果区
          <div className="dt-tabs">
            <button type="button" className={`dt-tab${detailsTab === "preview" ? " on" : ""}`} onClick={() => setDetailsTab("preview")}>
              预览
            </button>
            <button type="button" className={`dt-tab${detailsTab === "arts" ? " on" : ""}`} onClick={() => setDetailsTab("arts")}>
              产物
            </button>
            <button type="button" className={`dt-tab${detailsTab === "session" ? " on" : ""}`} onClick={() => setDetailsTab("session")}>
              会话
            </button>
          </div>
          <button type="button" className="icon-btn dt-close" title="收起面板" aria-label="收起面板" onClick={onToggleDetails}>
            <Icon name="x" />
          </button>
        </div>
        <div className="dt-scroll">
          {detailsTab === "preview" ? (
            previewPath ? (
              <PreviewDock
                path={previewPath}
                layout="chat"
                maximized={previewMax}
                onMaximize={setPreviewMax}
                onClose={() => {
                  setPreviewMax(false);
                  setPreviewPath("");
                }}
              />
            ) : (
              <div className="sub">本轮还没有打开的文件。点产物或聊天里的附件即可预览。</div>
            )
          ) : null}
          {detailsTab === "arts" || detailsTab === "preview" ? (
          <div className="dt-block">
            <div className="cap">{arts.length ? "本轮产物" : "本轮文件"}</div>
            {arts.length === 0 && previewList.length === 0 ? <div className="sub">本轮还没有写入的交付文件，也没有可预览的附件。</div> : null}
            {(arts.length ? arts : previewList).map((p) => (
              <div key={p} className={`artifact${isImagePath(p) ? " img" : ""}`}>
                {isImagePath(p) ? (
                  <img className="a-thumb" src={fileHref(p)} alt="" />
                ) : (
                  <div className="a-ico"><Icon name="file" /></div>
                )}
                <div className="grow">
                  <div className="a-name" style={{ fontFamily: "var(--mono)", fontSize: 12 }}>{basename(p)}</div>
                  <div className="sub">{p}</div>
                </div>
                <button
                  type="button"
                  className="btn ghost small"
                  onClick={() => openPreview(p)}
                >
                  预览
                </button>
                <a className="btn ghost small" href={fileHref(p, true)} download={basename(p)}>
                  下载
                </a>
              </div>
            ))}
          </div>
          ) : null}
          {detailsTab === "session" ? (
          <>
          <div className="dt-block">
            <div className="cap">当前会话</div>
            <div className="kv"><span>模式</span><b>{snap.mode || "—"}</b></div>
            <div className="kv"><span>审批</span><b>{snap.approvals || "—"}</b></div>
            <div className="kv"><span>作用域</span><b>{agentScope === "global" ? "全局" : "工作区"}</b></div>
            <div className="kv"><span>计划模式</span><b>{snap.plan_mode ? "on" : "off"}</b></div>
            <div className="kv"><span>AskQuestion</span><b>{snap.clarify_mode ? "on" : "off"}</b></div>
            <div className="kv"><span>生成图片</span><b>{snap.imagine_mode ? "on" : "off"}</b></div>
            <div className="kv"><span>忙碌策略</span><b>{snap.busy || "—"}</b></div>
            <div className="kv"><span>运行</span><b>{busy ? PHASE_LABEL[phase] : "空闲"}</b></div>
            <div className="kv"><span>排队</span><b>{queued}</b></div>
            <div className="kv"><span>转向</span><b>{steered}</b></div>
            <div className="kv"><span>低精度</span><b>{snap.low_precision ? "on" : "off"}</b></div>
          </div>
          {(snap.queue_preview?.length || snap.steer_preview?.length) ? (
            <div className="dt-block">
              <div className="cap">待处理</div>
              {(snap.queue_preview || []).map((t, i) => (
                <div className="sub" key={`q${i}`} style={{ marginBottom: 6 }}>排队 · {t}</div>
              ))}
              {(snap.steer_preview || []).map((t, i) => (
                <div className="sub" key={`s${i}`} style={{ marginBottom: 6 }}>转向 · {t}</div>
              ))}
            </div>
          ) : null}
          <div className="dt-block">
            <div className="cap">上下文窗口</div>
            <div className="gauge">
              <div className="g-label">
                <span>当前前缀 / 窗口</span>
                <b>
                  {used.toLocaleString()} / {win ? win.toLocaleString() : "—"} ({rawPct}%)
                </b>
              </div>
              <div className="g-track">
                <div className="g-fill" style={{ width: `${pct}%` }} />
              </div>
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              最近一次模型调用的 prompt。compact 之后是压缩后的活窗口，不是历史上每跳相加。
              {compacts ? ` 已 compact ${compacts} 次。` : ""}
            </div>
            {used >= 200_000 ? (
              <div className="banner warn hot" style={{ marginTop: 8, marginBottom: 0 }}>
                已超过 200k 价格悬崖：$2 / $0.50 / $6 → $4 / $1 / $12（每百万）。
              </div>
            ) : (
              <div className="sub" style={{ marginTop: 8 }}>
                前缀接近 200k 后单价翻倍 $2 / $0.50 / $6 → $4 / $1 / $12。
              </div>
            )}
          </div>
          <div className="dt-block">
            <div className="cap">累计用量</div>
            <div className="kv"><span>prompt</span><b>{(usage?.prompt_tokens ?? 0).toLocaleString()}</b></div>
            <div className="kv"><span>completion</span><b>{(usage?.completion_tokens ?? 0).toLocaleString()}</b></div>
            <div className="kv"><span>cached</span><b>{usageCachedReported(usage) ? (usage?.cached_tokens ?? 0).toLocaleString() : "n/a"}</b></div>
            <div className="kv"><span>前缀命中率</span><b>{hit}</b></div>
            <div className="kv"><span>assistant 步数</span><b>{usageSteps(usage)}</b></div>
            <div className="kv"><span>compact</span><b>{compacts}</b></div>
          </div>
          </>
          ) : null}
        </div>
      </aside>
      {planReview ? (
        <PlanReviewModal onStay={dismissPlanReview} onGo={() => void planImplement()} onExit={() => void planExit()} />
      ) : null}
      {lightbox ? <Lightbox src={lightbox} onClose={() => setLightbox("")} /> : null}
    </div>
  );
}

/* ── 轮次分组：把会话事件折成「用户消息 + 轨迹块 + 正文」 ────────── */

const HIDE_OPEN = "<tool_response>";
const HIDE_CLOSE = "</tool_response>";

/** Harness 注入的隐藏注记（守卫、提示、转向）以 tool_response 包裹存进 user 事件。 */
function hiddenNote(text: string): string | null {
  const t = text.trim();
  if (!t.startsWith(HIDE_OPEN) || !t.endsWith(HIDE_CLOSE)) return null;
  return t.slice(HIDE_OPEN.length, Math.max(HIDE_OPEN.length, t.length - HIDE_CLOSE.length)).trim();
}

function firstLine(s: string): string {
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (t) return t;
  }
  return "";
}

function clipEnd(s: string, n: number): string {
  const cs = [...s];
  return cs.length <= n ? s : `${cs.slice(0, n - 1).join("")}…`;
}

/** 直播思考里“正在说的那句话”：取末行的尾段。 */
function thinkTail(s: string): string {
  const t = s.trimEnd();
  const nl = t.lastIndexOf("\n");
  const line = (nl >= 0 ? t.slice(nl + 1) : t).trim();
  const cs = [...line];
  return cs.length <= 64 ? line : `…${cs.slice(-63).join("")}`;
}

type ToolStep = {
  kind: "tool";
  id: string;
  name: string;
  args: string;
  output?: string;
  done: boolean;
  media?: StoredMedia[];
};
type Step = ToolStep | { kind: "think"; text: string; live?: boolean } | { kind: "note"; text: string };
type ActivityBlockData = { kind: "activity"; steps: Step[] };
type Block =
  | ActivityBlockData
  | { kind: "text"; text: string; live?: boolean; media?: StoredMedia[] }
  | { kind: "sys"; text: string };
type TurnGroup = {
  key: string;
  user?: string;
  userMedia?: StoredMedia[];
  blocks: Block[];
  /** 有引擎 timings 的 hop 加权平均 decode tok/s。 */
  decodeTokS: number | null;
};

/** djb2：给轮次生成内容稳定的 key，history.replace / compact 后不漂移。 */
function hashText(s: string): string {
  let h = 5381;
  for (let i = 0; i < s.length; i++) h = ((h << 5) + h + s.charCodeAt(i)) >>> 0;
  return h.toString(36);
}

function commonPrefixLen(a: string, b: string): number {
  const n = Math.min(a.length, b.length);
  let i = 0;
  while (i < n && a[i] === b[i]) i++;
  return i;
}

/** Same hop restating the previous think preamble (思考轨迹复读). */
function thinkRepeats(prev: string, next: string): boolean {
  const a = prev.trim();
  const b = next.trim();
  if (!a || !b) return false;
  if (a === b) return true;
  if (b.startsWith(a) || a.startsWith(b)) return true;
  return commonPrefixLen(a, b) >= 20;
}

function mergeThink(prev: string, next: string): string {
  const a = prev.trim();
  const b = next.trim();
  if (b.startsWith(a) || b.length >= a.length) return next;
  return prev;
}

function pushThink(steps: Step[], text: string, live?: boolean): Step[] {
  const out = steps.slice();
  for (let i = out.length - 1; i >= 0; i--) {
    const st = out[i];
    if (st.kind !== "think") continue;
    if (thinkRepeats(st.text, text)) {
      out[i] = { kind: "think", text: mergeThink(st.text, text), live: live ?? st.live };
      return out;
    }
    break;
  }
  out.push({ kind: "think", text, live });
  return out;
}

function buildTurns(events: SessionEvent[]): TurnGroup[] {
  const turns: TurnGroup[] = [];
  const seen = new Map<string, number>();
  let cur: TurnGroup | undefined;
  let act: ActivityBlockData | undefined;
  let ratedTok = 0;
  let decodeSec = 0;

  const turn = (): TurnGroup => {
    if (!cur) {
      cur = { key: "t-head", blocks: [], decodeTokS: null };
      turns.push(cur);
    }
    return cur;
  };
  const finishRates = () => {
    if (!cur) return;
    cur.decodeTokS = decodeSec > 0 ? ratedTok / decodeSec : null;
  };
  const noteDecode = (e: SessionEvent) => {
    const c = e.completion_tokens || 0;
    const r = e.decode_tok_s;
    if (c > 0 && typeof r === "number" && r > 0 && Number.isFinite(r)) {
      ratedTok += c;
      decodeSec += c / r;
    }
  };
  const activity = (): ActivityBlockData => {
    if (!act) {
      act = { kind: "activity", steps: [] };
      turn().blocks.push(act);
    }
    return act;
  };

  events.forEach((e, i) => {
    switch (e.type) {
      case "user": {
        const note = hiddenNote(e.text || "");
        if (note !== null) {
          if (note) activity().steps.push({ kind: "note", text: note });
          return;
        }
        finishRates();
        act = undefined;
        ratedTok = 0;
        decodeSec = 0;
        const media = userMediaFromEvent(e);
        const hv = hashText(`${e.text || ""}\0${media.map((m) => m.url).join("\0")}`);
        const n = (seen.get(hv) || 0) + 1;
        seen.set(hv, n);
        cur = {
          key: `u${hv}.${n}`,
          user: stripAttachedNotes(e.text || ""),
          userMedia: media,
          blocks: [],
          decodeTokS: null,
        };
        turns.push(cur);
        return;
      }
      case "assistant": {
        noteDecode(e);
        if (e.reasoning) {
          const act = activity();
          act.steps = pushThink(act.steps, e.reasoning);
        }
        const media = parseStoredMedia(e.media);
        if (e.content || media.length) {
          act = undefined;
          turn().blocks.push({ kind: "text", text: e.content || "", media });
        }
        (e.tool_calls || []).forEach((c, j) => {
          activity().steps.push({
            kind: "tool",
            id: c.id || `${i}.${j}`,
            name: c.function?.name || "tool",
            args: c.function?.arguments || "",
            done: false,
          });
        });
        return;
      }
      case "tool": {
        const id = e.tool_call_id;
        let hit: ToolStep | undefined;
        const blocks = turn().blocks;
        outer: for (let b = blocks.length - 1; b >= 0; b--) {
          const blk = blocks[b];
          if (blk.kind !== "activity") continue;
          for (let s = blk.steps.length - 1; s >= 0; s--) {
            const st = blk.steps[s];
            if (st.kind === "tool" && !st.done && (!id || st.id === id)) {
              hit = st;
              break outer;
            }
          }
        }
        if (!hit) {
          hit = { kind: "tool", id: id || `r${i}`, name: e.name || "tool", args: "", done: false };
          activity().steps.push(hit);
        }
        hit.done = true;
        hit.output = e.output || "";
        const media = parseStoredMedia(e.media);
        if (media.length) hit.media = media;
        return;
      }
      case "stop": {
        act = undefined;
        // 常规收束不占一行。物理上限（步数/时间/上下文）已改成安静收束，
        // 只把中止和真正的错误露给用户。
        const reason = (e.reason || "").trim();
        if (reason && !quietStopReason(reason)) {
          turn().blocks.push({ kind: "sys", text: reason });
        }
        return;
      }
      case "session/compact":
        act = undefined;
        turn().blocks.push({ kind: "sys", text: "上下文已压缩，早期轨迹已归档。" });
        return;
      default: {
        const t = (e.type || "").toLowerCase();
        if (t === "todo" || t === "todo_write" || t === "todos") {
          const n = parseTodos(JSON.stringify(e)).length;
          activity().steps.push({
            kind: "note",
            text: n ? `TodoWrite · ${n} 项` : "TodoWrite",
          });
          return;
        }
        if (t === "subagent" || t === "task" || t === "agent") {
          const id = strField(e, "subagent_id", "agent_id", "task_id", "id") || "subagent";
          const label = strField(e, "name", "label", "description") || "子代理";
          const status = strField(e, "status") || "running";
          activity().steps.push({
            kind: "note",
            text: `子代理 ${label} · ${id} · ${status}`,
          });
        }
        return;
      }
    }
  });
  finishRates();
  for (const t of turns) dropHostImageToolIfShot(t);
  return turns;
}

/** Imagine REST / host image_generation used to persist a tool card AND the
 *  assistant shot. Once the thumb is in the bubble, drop the extra row. */
function dropHostImageToolIfShot(turn: TurnGroup) {
  const hasShot = turn.blocks.some(
    (b) => b.kind === "text" && Boolean(b.media && b.media.length),
  );
  if (!hasShot) return;
  turn.blocks = turn.blocks
    .map((b) => {
      if (b.kind !== "activity") return b;
      return {
        ...b,
        steps: b.steps.filter((s) => !(s.kind === "tool" && isHostImageTool(s.name))),
      };
    })
    .filter((b) => b.kind !== "activity" || b.steps.length > 0);
}

/** 单个轮次。memo 后流式增量只触发最后一轮重渲，长会话打字不再整屏刷新。 */
const TurnView = memo(function TurnView({
  turnKey,
  user,
  userMedia,
  blocks,
  active,
  decodeTokS,
  callLabel,
  elapsed,
  openBlocks,
  openSteps,
  onToggleBlock,
  onToggleStep,
  onOpenPreview,
}: {
  turnKey: string;
  user?: string;
  userMedia?: StoredMedia[];
  blocks: Block[];
  active: boolean;
  decodeTokS: number | null;
  callLabel: string;
  elapsed: number;
  openBlocks: Set<string>;
  openSteps: Set<string>;
  onToggleBlock: (k: string) => void;
  onToggleStep: (k: string) => void;
  onOpenPreview?: (path: string) => void;
}) {
  const answer = blocks
    .filter((b): b is { kind: "text"; text: string; live?: boolean; media?: StoredMedia[] } => b.kind === "text")
    .map((b) => b.text)
    .filter(Boolean)
    .join("\n\n");
  const hasUser = Boolean(user) || Boolean(userMedia && userMedia.length > 0);
  return (
    <section className="turn" data-turn={turnKey}>
      {hasUser ? (
        <div className="msg user">
          {userMedia && userMedia.length ? <MediaStrip items={userMedia} onOpen={onOpenPreview} /> : null}
          {user ? <div className="bubble">{user}</div> : null}
        </div>
      ) : null}
      {blocks.map((b, bi) => {
        const bk = `${turnKey}:${bi}`;
        if (b.kind === "text") {
          const shots = b.media?.length ? (
            <div className="ans-media">
              <MediaStrip items={b.media} onOpen={onOpenPreview} />
            </div>
          ) : null;
          if (!b.text && !shots) return null;
          if (!b.text) {
            return (
              <div key={bk} className={`msg-a${b.live ? " caret" : ""}`}>
                {shots}
              </div>
            );
          }
          return (
            <div key={bk}>
              <MdText text={b.text} live={b.live} />
              {shots}
            </div>
          );
        }
        if (b.kind === "sys") {
          return (
            <div key={bk} className="turn-sys">
              {b.text}
            </div>
          );
        }
        const running = active && bi === blocks.length - 1;
        return (
          <ActivityBlock
            key={bk}
            bk={bk}
            steps={b.steps}
            running={running}
            callLabel={callLabel}
            elapsed={elapsed}
            open={openBlocks.has(bk)}
            openSteps={openSteps}
            onToggle={onToggleBlock}
            onToggleStep={onToggleStep}
          />
        );
      })}
      <TurnFoot answer={answer} decodeTokS={decodeTokS} />
    </section>
  );
});

function TurnFoot({ answer, decodeTokS }: { answer: string; decodeTokS: number | null }) {
  const [copied, setCopied] = useState(false);
  if (!answer && decodeTokS == null) return null;
  return (
    <div className="turn-foot">
      {answer ? (
        <button
          type="button"
          className="copy-ans"
          title="复制回答全文"
          aria-label="复制回答全文"
          onClick={() => {
            void copyText(answer).then((ok) => {
              if (!ok) return;
              setCopied(true);
              window.setTimeout(() => setCopied(false), 1400);
            });
          }}
        >
          <Icon name={copied ? "check" : "copy"} />
        </button>
      ) : null}
      {decodeTokS != null ? (
        <span className="toks" title="本轮解码平均速度（completion tokens / decode 时间）">
          {fmtTokS(decodeTokS)}
        </span>
      ) : null}
    </div>
  );
}

function actSummary(steps: Step[]): string {
  let think = 0;
  let tool = 0;
  let note = 0;
  for (const s of steps) {
    if (s.kind === "think") think++;
    else if (s.kind === "tool") tool++;
    else note++;
  }
  const bits: string[] = [];
  if (think > 0) bits.push(think === 1 ? "思考" : `思考 ${think} 段`);
  if (tool > 0) bits.push(`工具 ${tool} 次`);
  if (bits.length === 0) return note > 0 ? "系统注记" : "轨迹";
  return bits.join(" · ");
}

function argPreview(name: string, raw: string): string {
  let a: Record<string, unknown> = {};
  try {
    a = JSON.parse(raw || "{}") as Record<string, unknown>;
  } catch {
    return clipEnd(firstLine(raw), 64);
  }
  const s = (k: string) => (typeof a[k] === "string" ? (a[k] as string) : "");
  switch (toolKey(name)) {
    case "read":
    case "view":
    case "write":
    case "edit":
    case "strreplace":
    case "delete":
      return s("path");
    case "bash":
    case "shell":
      return clipEnd(firstLine(s("command")), 64);
    case "runcode":
      return clipEnd(firstLine(s("code")), 64);
    case "web":
    case "websearch":
      return clipEnd(s("search_term") || s("query") || s("url"), 64);
    case "webfetch":
      return clipEnd(s("url"), 64);
    case "search":
    case "memorysearch":
    case "recall":
    case "grep":
      return clipEnd(s("pattern") || s("query"), 64);
    case "glob":
      return clipEnd(s("glob_pattern") || s("pattern") || s("target_directory"), 64);
    case "todowrite":
    case "todo": {
      const n = parseTodos(raw).length;
      return n ? `${n} 项` : clipEnd(s("merge") || firstLine(raw), 64);
    }
    case "askquestion":
    case "ask":
      return clipEnd(s("title") || s("prompt") || s("question"), 64);
    case "task":
      return clipEnd(s("description") || s("subagent_type") || s("prompt"), 64);
    case "skill":
      return s("name");
    case "mcp":
      return [s("server"), s("method")].filter(Boolean).join(" · ");
    case "imagegeneration":
      return clipEnd(s("prompt") || firstLine(raw), 64);
    default: {
      const v = Object.values(a).find((x) => typeof x === "string") as string | undefined;
      return clipEnd(firstLine(v || ""), 64);
    }
  }
}

function quietStopReason(reason: string): boolean {
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

function toolIcon(name: string): string {
  const n = toolKey(name);
  if (n === "read" || n === "view") return "book";
  if (n === "edit" || n === "write" || n === "strreplace" || n === "delete") return "edit";
  if (n === "bash" || n === "shell" || n === "runcode") return "terminal";
  if (n === "web" || n === "websearch" || n === "webfetch") return "globe";
  if (n === "mcp") return "plug";
  if (n === "search" || n === "memorysearch" || n === "recall" || n === "grep" || n === "glob") return "search";
  if (n === "skill" || n === "task") return "spark";
  if (n === "imagegeneration") return "image";
  if (n === "todowrite" || n === "todo" || n === "askquestion" || n === "ask") return "list";
  return "wrench";
}

function stepStatus(s: ToolStep): "run" | "ok" | "err" | "warn" {
  if (!s.done) return "run";
  const out = (s.output || "").trimStart();
  if (out === "tool task aborted") return "warn";
  if (/^(error|错误)/i.test(out)) return "err";
  return "ok";
}

function fmtArgs(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

function StepRow({
  step,
  sk,
  open,
  onToggle,
}: {
  step: Step;
  sk: string;
  open: boolean;
  onToggle: (k: string) => void;
}) {
  if (step.kind === "think") {
    return (
      <div className="step think">
        <button type="button" className="step-head" onClick={() => onToggle(sk)} aria-expanded={open}>
          <Icon name="spark" />
          <span className="step-name">思考</span>
          <span className="step-prev">
            {step.live && !open ? thinkTail(step.text) : clipEnd(firstLine(step.text), 80)}
          </span>
          {step.live ? <span className="st-dot run" /> : null}
        </button>
        {open ? <div className="step-full think-full">{step.text}</div> : null}
      </div>
    );
  }
  if (step.kind === "note") {
    return (
      <div className="step note">
        <button type="button" className="step-head" onClick={() => onToggle(sk)} aria-expanded={open}>
          <Icon name="shield" />
          <span className="step-name">注记</span>
          <span className="step-prev">{clipEnd(firstLine(step.text), 80)}</span>
        </button>
        {open ? <div className="step-full think-full">{step.text}</div> : null}
      </div>
    );
  }
  const st = stepStatus(step);
  const todoItems = isTodoTool(step.name)
    ? parseTodos(step.args).length
      ? parseTodos(step.args)
      : parseTodos(step.output || "")
    : [];
  return (
    <div className="step tool">
      <button type="button" className="step-head" onClick={() => onToggle(sk)} aria-expanded={open}>
        <Icon name={toolIcon(step.name)} />
        <span className="step-name mono">{toolLabel(step.name)}</span>
        <span className="step-prev">{argPreview(step.name, step.args) || clipEnd(firstLine(step.output || ""), 80)}</span>
        <span className={`st-dot ${st}`} />
      </button>
      {open ? (
        <div className="step-full">
          {todoItems.length ? <TodoBoard items={todoItems} /> : null}
          {step.media?.length ? <MediaStrip items={step.media} onOpen={onOpenPreview} /> : null}
          {step.args ? <pre className="pre step-pre">{fmtArgs(step.args)}</pre> : null}
          {step.done ? (
            step.output ? (
              <pre className="pre step-pre">{step.output}</pre>
            ) : (
              <div className="sub">（无输出）</div>
            )
          ) : (
            <div className="sub">
              {isTaskTool(step.name)
                ? "子代理运行中…"
                : isAskTool(step.name)
                  ? "AskQuestion 等待选择…"
                  : "运行中…"}
            </div>
          )}
        </div>
      ) : null}
    </div>
  );
}

function ActivityBlock({
  bk,
  steps,
  running,
  callLabel,
  elapsed,
  open,
  openSteps,
  onToggle,
  onToggleStep,
}: {
  bk: string;
  steps: Step[];
  running: boolean;
  callLabel: string;
  elapsed: number;
  open: boolean;
  openSteps: Set<string>;
  onToggle: (k: string) => void;
  onToggleStep: (k: string) => void;
}) {
  const last = steps[steps.length - 1];
  let label: string;
  let preview = "";
  const liveThink = [...steps]
    .reverse()
    .find((s): s is Extract<Step, { kind: "think" }> => s.kind === "think" && !!s.live);
  if (running) {
    if (liveThink) {
      label = "思考中";
      preview = thinkTail(liveThink.text);
    } else if (last?.kind === "tool" && !last.done) {
      if (isTaskTool(last.name)) label = "子代理运行中";
      else if (isTodoTool(last.name)) label = "TodoWrite";
      else if (isAskTool(last.name)) label = "AskQuestion";
      else label = `运行 ${last.name}`;
      preview = argPreview(last.name, last.args);
    } else {
      label = callLabel;
    }
  } else {
    label = actSummary(steps);
  }
  const canOpen = steps.length > 0;
  const expanded = open && canOpen;
  return (
    <div className={`activity${expanded ? " open" : ""}${running ? " running" : ""}`}>
      <button
        type="button"
        className="act-head"
        onClick={() => canOpen && onToggle(bk)}
        aria-expanded={expanded}
        disabled={!canOpen}
      >
        {canOpen ? <Icon name="chev-r" className="ico act-chev" /> : null}
        {running ? <span className="act-spin" aria-hidden /> : null}
        <span className={`act-label${running ? " shimmer" : ""}`}>{label}</span>
        {preview && !expanded ? <span className="act-prev">{preview}</span> : null}
        {running && elapsed > 0 ? <span className="act-time">{fmtElapsed(elapsed)}</span> : null}
      </button>
      {expanded ? (
        <div className="act-trace">
          {steps.map((s, si) => {
            const sk = `${bk}:${si}`;
            return <StepRow key={sk} step={s} sk={sk} open={openSteps.has(sk)} onToggle={onToggleStep} />;
          })}
        </div>
      ) : null}
    </div>
  );
}

export function PlanReviewModal({
  onStay,
  onGo,
  onExit,
}: {
  onStay: () => void;
  onGo: () => void;
  onExit: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      onStay();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onStay]);

  return (
    <Overlay onClose={() => {}}>
      <div className="modal" role="dialog" aria-modal="true" aria-labelledby="plan-review-title">
        <h2 id="plan-review-title">
          <Icon name="list" />
          计划已就绪
        </h2>
        <div className="m-sub">plan · 本轮已完成</div>
        <div className="sub" style={{ marginBottom: 14 }}>
          批准后退出计划模式并开始实施。继续规划保持只读；退出计划则关掉计划模式。
        </div>
        <div className="m-actions">
          <button type="button" className="btn ghost" onClick={onExit}>
            退出计划
          </button>
          <button type="button" className="btn ink" onClick={onStay}>
            继续规划
          </button>
          <button type="button" className="btn primary" onClick={onGo}>
            批准实施
          </button>
        </div>
      </div>
    </Overlay>
  );
}

export function ClarifyModal({
  clarify,
  onClose,
}: {
  clarify: NonNullable<Clarify>;
  onClose: () => void;
}) {
  const [otherOpen, setOtherOpen] = useState(false);
  const [other, setOther] = useState("");
  const go = async (body: Record<string, unknown>) => {
    await api("/clarify", { method: "POST", body: JSON.stringify({ id: clarify.id, ...body }) });
    onClose();
  };
  const submitOther = () => {
    const text = other.trim();
    void go(text ? { other: text } : { skip: true });
  };
  // 与审批卡相同：点遮罩 / Esc 不动作，只有选项、其他、跳过能结束阻塞。
  return (
    <Overlay onClose={() => {}}>
      <div className="modal" role="dialog" aria-labelledby="clarify-title">
        <h2 id="clarify-title">
          <Icon name="list" />
          {clarify.title || "AskQuestion · 请选择"}
        </h2>
        <div className="m-sub">AskQuestion · clarify.ask · id #{clarify.id}</div>
        {clarify.prompt ? <div className="sub" style={{ marginBottom: 14 }}>{clarify.prompt}</div> : null}
        {otherOpen ? (
          <>
            <input
              className="input"
              value={other}
              onChange={(e) => setOther(e.target.value)}
              placeholder="输入其他说明"
              aria-label="其他说明"
              autoFocus
              onKeyDown={(e) => {
                if (e.key === "Enter") submitOther();
              }}
            />
            <div className="m-actions" style={{ marginTop: 12 }}>
              <button type="button" className="btn ghost" onClick={() => setOtherOpen(false)}>返回</button>
              <button type="button" className="btn primary" onClick={submitOther}>提交</button>
            </div>
          </>
        ) : (
          <>
            <div className="m-actions col">
              {clarify.options.map((o, i) => (
                <button
                  key={o.id}
                  type="button"
                  className={i === 0 ? "btn primary" : "btn"}
                  onClick={() => void go({ pick: o.id })}
                >
                  {i === 0 ? `${o.label} · 推荐` : o.label}
                </button>
              ))}
            </div>
            <div className="m-actions">
              <button type="button" className="btn ghost" onClick={() => void go({ skip: true })}>跳过</button>
              <button type="button" className="btn ink" onClick={() => setOtherOpen(true)}>其他</button>
            </div>
          </>
        )}
      </div>
    </Overlay>
  );
}

export function PermitModal({
  permit,
  onClose,
}: {
  permit: NonNullable<Permit>;
  onClose: () => void;
}) {
  const go = async (d: string) => {
    await api("/permit", { method: "POST", body: JSON.stringify({ id: permit.id, decision: d }) });
    onClose();
  };
  // 审批是不可逆裁决：点遮罩 / Esc 一律不动作，只有三个按钮能裁决。
  return (
    <Overlay onClose={() => {}}>
      <div className="modal" role="dialog" aria-labelledby="permit-title">
        <h2 id="permit-title">
          <Icon name="shield" />
          工具调用审批
          <span className={`tc-badge ${toolBadge(permit.tool)}`} style={{ marginLeft: "auto" }}>
            {permit.tool}
          </span>
        </h2>
        <div className="m-sub">permit.ask · id #{permit.id}</div>
        <pre className="pre" style={{ marginBottom: 12, maxHeight: 150 }}>{permit.preview}</pre>
        <div className="sub" style={{ marginBottom: 14 }}>
          allow 放行这一次 · always 本进程记住该工具 · deny 拒绝。点空白处不会裁决；中止轮次视为 deny。
        </div>
        <div className="m-actions">
          <button className="btn danger" onClick={() => go("deny")}>拒绝</button>
          <button className="btn ink" onClick={() => go("always")}>始终允许</button>
          <button className="btn primary" onClick={() => go("allow")}>允许</button>
        </div>
      </div>
    </Overlay>
  );
}

export function fmtAgo(ts?: number | null) {
  if (!ts) return "—";
  const s = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (s < 60) return `${s} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return new Date(ts * 1000).toLocaleString("zh-CN");
}
