import type { SessionEvent } from "./api";
import { pathFromMediaUrl } from "./media";

/**
 * Artifact list follows Cursor's file-review source, then office display rank.
 *
 * Cursor (`fileStatesV2` / `onDidAiEditFile`): only files the agent actually
 * mutated — Write / StrReplace / Delete, plus Shell I/O (redirects, -o, stdout
 * paths). Read / Grep / Glob / chat prose are not a file list. Ignored dirs
 * match Cursor's browse skip set (`.git`, `node_modules`, `dist`, `build`, …).
 *
 * Hyper is not an IDE: when those writes include an office/preview file, the
 * 产物 rail shows that file and hides process scripts (`.py`, unpack listings).
 */

const WRITE_TOOLS = new Set(["write", "edit", "strreplace", "delete"]);

const JUNK_DIR = [
  "/library/python/",
  "/site-packages/",
  "/dist-packages/",
  "/__pycache__/",
  "/node_modules/",
  "/.git/",
  "/dist/",
  "/build/",
  "/.next/",
  "/.nuxt/",
  "/.cache/",
  "/target/debug/",
  "/target/release/",
  "/usr/lib/",
  "/opt/homebrew/lib/",
  "/.hyper-sdk/",
  "/.grok-hyper/",
  "/.pptx-venv/",
  "/.venv/",
  "/venv/",
];

const SCRATCH_NAMES = new Set([
  "analysis",
  "analysis.md",
  "scratch.md",
  "dump.md",
  "tmp.md",
  "probe.txt",
]);

/** Deliverable extensions — keep in sync with preview kinds (office + canvas/visio/html/image). */
const PRODUCT_EXT_SRC =
  "pptx|ppt|ppsx|pptm|odp|docx|docm|doc|odt|rtf|xlsx|xlsm|xls|ods|pdf|html?|png|jpe?g|gif|webp|svg|bmp|csv|vsdx|vsd|canvas\\.json";
const PRODUCT_EXT = new RegExp(`\\.(?:${PRODUCT_EXT_SRC})$`, "i");
const PREVIEW_EXT =
  /\.(pptx?|ppsx|pptm|odp|docx?|docm|odt|rtf|xlsx?|xlsm|ods|pdf|html?|png|jpe?g|gif|webp|svg|bmp|csv|vsdx?|canvas\.json|md|txt)$/i;
/** One path segment: CJK names like 使用说明.docx. Spaces only allowed inside quotes. */
const PATH_SEG = String.raw`[^\s'"<>()[\]|\\/]+`;
const PROCESS_EXT = /\.(py|swift|sh|bash|zsh|js|mjs|cjs|ts|tsx|rb|pl)$/i;
const PROCESS_DIR = [
  "/ppt-preview/",
  "/.pptx-venv/",
  "/.venv/",
  "/venv/",
  "/__pycache__/",
  "/word/media/",
  "/word/embeddings/",
  "/ppt/media/",
  "/ppt/slides/",
  "/ppt/slideLayouts/",
  "/ppt/slideMasters/",
  "/xl/media/",
  "/xl/worksheets/",
  "/xl/drawings/",
  "/visio/pages/",
];

function norm(p: string) {
  return p.replace(/\\/g, "/").replace(/\/+/g, "/").trim();
}

/** macOS `/tmp` and `/private/tmp` are the same directory. */
export function foldPath(p: string): string {
  return norm(p).replace(/^\/private\/(tmp|var)\//, "/$1/");
}

function basename(p: string) {
  const n = foldPath(p).replace(/\/+$/, "");
  const i = n.lastIndexOf("/");
  return (i >= 0 ? n.slice(i + 1) : n).toLowerCase();
}

function parseArgs(raw: unknown): Record<string, unknown> {
  if (!raw) return {};
  if (typeof raw === "object" && !Array.isArray(raw)) {
    return raw as Record<string, unknown>;
  }
  if (typeof raw !== "string") return {};
  try {
    const v = JSON.parse(raw);
    if (v && typeof v === "object" && !Array.isArray(v)) return v as Record<string, unknown>;
  } catch {
    /* XML / truncated args */
  }
  return {};
}

function argPath(args: Record<string, unknown>): string | null {
  for (const k of ["path", "file_path", "new_path"]) {
    const v = args[k];
    if (typeof v === "string" && v.trim()) return v.trim();
  }
  return null;
}

function isShellTool(name: string) {
  return name === "bash" || name === "shell";
}

export function isJunkPath(p: string) {
  const n = `/${foldPath(p).toLowerCase()}/`;
  if (JUNK_DIR.some((d) => n.includes(d))) return true;
  const base = basename(p);
  if (base.endsWith(".log") || base === ".ds_store") return true;
  if (base.includes(".hypertmp.") || base.endsWith(".hypertmp")) return true;
  if (base.startsWith(".grok-hyper")) return true;
  if (base.endsWith(".tmp") || base.endsWith(".swp") || base.endsWith("~")) return true;
  return p === "/dev/null" || base === "dev/null";
}

export function isGeneratedPath(p: string) {
  return `/${foldPath(p).toLowerCase()}/`.includes("/.grok-hyper/generated/");
}

function isScratchName(p: string) {
  const base = basename(p);
  if (SCRATCH_NAMES.has(base)) return true;
  const n = foldPath(p).toLowerCase();
  return /(^|\/)notes\/harness_/.test(n) || /(^|\/)reports\/harness_/.test(n);
}

function isRemoteUrl(p: string) {
  const n = p.trim();
  if (/:\/\//.test(n)) return true;
  // After slash-fold, `https://x` becomes `https:/x`.
  if (/^(https?|s?ftp|file|data):/i.test(n)) return true;
  return false;
}

function inWorkspace(p: string, workspace?: string) {
  const n = foldPath(p);
  if (!n || n === "/dev/null") return false;
  const abs = n.startsWith("/") || /^[a-zA-Z]:\//.test(n);
  if (!abs) return !n.startsWith("../");
  if (!workspace) return true;
  const w = foldPath(workspace).replace(/\/$/, "");
  return n === w || n.startsWith(`${w}/`);
}

export function isDeliverablePath(p: string, workspace?: string) {
  const n = foldPath(p).replace(/^['"`]|['"`]$/g, "");
  if (!n || n.length > 512) return false;
  // Shell fragments: DOC="a.docx", `cd /x && ./x2t a.docx`.
  if (/[=;&|<>"]/.test(n)) return false;
  if (isRemoteUrl(n)) return false;
  if (isGeneratedPath(n)) return inWorkspace(n, workspace);
  if (isJunkPath(n) || isScratchName(n)) return false;
  return inWorkspace(n, workspace);
}

function isProcessPath(p: string): boolean {
  const n = `/${foldPath(p).toLowerCase()}/`;
  if (PROCESS_DIR.some((d) => n.includes(d))) return true;
  const base = basename(p);
  // `word/guide.docx` is a deliverable; `word/document.xml` is an OOXML listing.
  if (PRODUCT_EXT.test(base)) return false;
  if (PROCESS_EXT.test(base)) return true;
  if (base.endsWith("-outline.json") || base.endsWith("_outline.json") || base === "outline.json") {
    return true;
  }
  if (/^(word|ppt|xl|_rels|docprops|visio)\//i.test(foldPath(p))) return true;
  return false;
}

function isProductPath(p: string): boolean {
  if (isProcessPath(p)) return false;
  return PRODUCT_EXT.test(basename(p)) || PRODUCT_EXT.test(foldPath(p));
}

function productRank(p: string): number {
  const n = foldPath(p).toLowerCase();
  if (/\.(pptx|ppt|ppsx|pptm|odp)$/.test(n)) return 100;
  if (/\.(docx|docm|doc|odt|rtf)$/.test(n)) return 90;
  if (/\.(xlsx|xlsm|xls|ods)$/.test(n)) return 90;
  if (/\.pdf$/.test(n)) return 80;
  if (/\.html?$/.test(n)) return 70;
  if (/\.canvas\.json$/.test(n) || /\.vsdx$|\.vsd$/.test(n)) return 60;
  if (/\.(png|jpe?g|gif|webp|svg|bmp)$/.test(n)) return 50;
  if (/\.csv$/.test(n)) return 40;
  return 10;
}

function toDisplayPath(p: string, workspace?: string): string {
  const n = foldPath(p);
  const w = foldPath(workspace || "").replace(/\/$/, "");
  if (w && (n === w || n.startsWith(`${w}/`))) {
    const rel = n.slice(w.length).replace(/^\//, "");
    return rel || n;
  }
  return p.replace(/\\/g, "/");
}

function addPath(out: Map<string, string>, raw: string | null | undefined, workspace?: string) {
  if (!raw) return;
  const trimmed = raw.replace(/^['"`]|['"`]$/g, "").replace(/\.$/, "");
  if (isRemoteUrl(trimmed)) return;
  const p = foldPath(trimmed);
  if (!isDeliverablePath(p, workspace)) return;
  const shown = toDisplayPath(p, workspace);
  const key = foldPath(shown).toLowerCase();
  if (!out.has(key)) out.set(key, shown);
}

function looksLikeFilePath(p: string): boolean {
  if (!p || p === "/dev/null") return false;
  if (/[=;&|<>]/.test(p)) return false;
  if (/[:*{}]$/.test(p)) return false;
  if (p.includes("/")) return true;
  return /\.[A-Za-z0-9]{1,12}$/.test(p);
}

function bashRedirects(cmd: string): string[] {
  const found: string[] = [];
  const re = /(?:^|[^\d\w-])(?:>>?|tee(?:\s+-a)?)\s+['"]?([^\s'";|&]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(cmd))) {
    const p = m[1];
    if (looksLikeFilePath(p)) found.push(p);
  }
  return found;
}

function copyDests(cmd: string): string[] {
  const found: string[] = [];
  const re = /(?:^|[;&|\n])\s*(?:cp|mv)(?:\s+-[A-Za-z]+)*\s+([^\n;&|]+)/gi;
  let m: RegExpExecArray | null;
  while ((m = re.exec(cmd))) {
    const args = m[1].trim();
    const quoted = [...args.matchAll(/['"]([^'"]+)['"]/g)].map((x) => x[1]);
    const last = quoted[quoted.length - 1] || args.trim().split(/\s+/).pop() || "";
    const p = last.replace(/^['"]|['"]$/g, "");
    if (p && PRODUCT_EXT.test(p)) found.push(p);
  }
  return found;
}

function announcedPaths(text: string): string[] {
  const found: string[] = [];
  const patterns = [
    /Wrote \d+ bytes to ([^\n]+?)\.?\s*$/gm,
    /Successfully replaced text in ([^\n]+?)\.?\s*$/gm,
    /(?:已保存|已写入|saved to|saved:|wrote to|written to)\s*[:：]?\s*`?([^\s`\n]+)/gi,
    /(?:^|\n)wrote\s+(\/[^\s\n]+)/gi,
  ];
  for (const re of patterns) {
    const copy = new RegExp(re.source, re.flags);
    let m: RegExpExecArray | null;
    while ((m = copy.exec(text))) found.push(m[1]);
  }
  return found;
}

/** Rooted paths. `\w` is ASCII-only, so Chinese filenames need a wider segment class. */
const PRODUCT_PATH_RE = new RegExp(
  String.raw`(?:^|[\s='"([<,:]|->)((?:\/|~\/|\.\/)(?:${PATH_SEG}\/)+${PATH_SEG}\.(?:${PRODUCT_EXT_SRC}))`,
  "gi",
);
const BACKTICK_PRODUCT_RE = new RegExp(`\`([^\\\`\\n]+\\.(?:${PRODUCT_EXT_SRC}))\``, "gi");
/** Whole quoted string that *is* a product path (allows spaces: `"Q3 报告.docx"`). */
const QUOTED_PRODUCT_RE = new RegExp(`['"]([^'"\\n]+\\.(?:${PRODUCT_EXT_SRC}))['"]`, "gi");
const SAVE_CALL_RE = new RegExp(
  String.raw`(?:\.save|\.savefig|to_excel|to_csv|to_html|to_markdown)\(\s*[rf]?['"]([^'"]+\.(?:${PRODUCT_EXT_SRC}))['"]`,
  "gi",
);
const TAIL_PATH_RE = new RegExp(
  String.raw`(^|\s)((?:\/|~\/|\.\/)${PATH_SEG}(?:\/${PATH_SEG})*\.(?:${PRODUCT_EXT_SRC}))\s*$`,
  "gm",
);

function pushMatches(re: RegExp, text: string, group: number, into: string[]) {
  const copy = new RegExp(re.source, re.flags);
  let m: RegExpExecArray | null;
  while ((m = copy.exec(text))) {
    if (m[group]) into.push(m[group]);
  }
}

function productPathsInText(text: string): string[] {
  const found: string[] = [];
  pushMatches(PRODUCT_PATH_RE, text, 1, found);
  pushMatches(BACKTICK_PRODUCT_RE, text, 1, found);
  pushMatches(QUOTED_PRODUCT_RE, text, 1, found);
  pushMatches(SAVE_CALL_RE, text, 1, found);
  pushMatches(TAIL_PATH_RE, text, 2, found);
  const dash = /(?:^|\s)(?:-o|--output|--outfile|--out|-out)\s+['"]?([^\s'"]+)/gi;
  let d: RegExpExecArray | null;
  while ((d = dash.exec(text))) {
    const p = d[1];
    if (PRODUCT_EXT.test(p)) found.push(p);
  }
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim().replace(/^['"]|['"]$/g, "");
    if (!t || /\s/.test(t) || /[=;&|<>]/.test(t)) continue;
    // Bare `new.pptx` from a failed command is not a file list; need a directory.
    if (PRODUCT_EXT.test(t) && t.includes("/") && looksLikeFilePath(t)) found.push(t);
  }
  return found;
}

/** Last live user message — skip hidden `<tool_response>` / steer notes. */
export function lastLiveUserIndex(events: SessionEvent[]): number {
  let start = 0;
  for (let i = 0; i < events.length; i++) {
    const e = events[i];
    if (e.type !== "user") continue;
    const t = String(e.text || e.content || "").trim();
    if (!t || t.startsWith("<tool_response>") || t.startsWith("Steer:")) continue;
    start = i;
  }
  return start;
}

function addProductMentions(out: Map<string, string>, text: string, workspace?: string) {
  productPathsInText(text).forEach((p) => addPath(out, p, workspace));
}

function addEventMedia(out: Map<string, string>, e: SessionEvent, workspace?: string) {
  const media = e.media;
  if (!Array.isArray(media)) return;
  for (const item of media) {
    if (!item || typeof item !== "object") continue;
    const url = String((item as { url?: string; image_url?: string }).url
      || (item as { image_url?: string }).image_url
      || "");
    const p = pathFromMediaUrl(url);
    if (p) addPath(out, p, workspace);
  }
}

function collectRaw(events: SessionEvent[], workspace?: string): string[] {
  const out = new Map<string, string>();
  const slice = events.slice(lastLiveUserIndex(events));
  for (const e of slice) {
    addEventMedia(out, e, workspace);
    if (e.type === "assistant") {
      for (const c of e.tool_calls || []) {
        const name = (c.function?.name || "").toLowerCase();
        const args = parseArgs(c.function?.arguments);
        if (WRITE_TOOLS.has(name)) addPath(out, argPath(args), workspace);
        if (isShellTool(name) && typeof args.command === "string") {
          bashRedirects(args.command).forEach((p) => addPath(out, p, workspace));
          copyDests(args.command).forEach((p) => addPath(out, p, workspace));
          addProductMentions(out, args.command, workspace);
        }
      }
    }
    if (e.type === "tool") {
      const name = (e.name || "").toLowerCase();
      const output = String(e.output || "");
      if (WRITE_TOOLS.has(name) || isShellTool(name)) {
        announcedPaths(output).forEach((p) => addPath(out, p, workspace));
        if (isShellTool(name)) addProductMentions(out, output, workspace);
      }
    }
  }
  return [...out.values()];
}

/** Write/save paths for the current user turn. Process scripts and unpack listings stay out. */
export function turnArtifacts(events: SessionEvent[], workspace?: string): string[] {
  const raw = collectRaw(events, workspace);
  const products = raw
    .filter(isProductPath)
    .sort((a, b) => productRank(b) - productRank(a) || a.localeCompare(b));
  if (products.length) return products;
  return raw.filter((p) => !isProcessPath(p));
}

export function isUploadPath(p: string): boolean {
  return `/${foldPath(p).toLowerCase()}/`.includes("/.grok-hyper/uploads/");
}

export function userAttachedPaths(events: SessionEvent[]): string[] {
  const e = events[lastLiveUserIndex(events)];
  if (!e || e.type !== "user") return [];
  const text = String(e.text || e.content || "");
  const out: string[] = [];
  const re = /\[attached: ([^\]]+)\]/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    const p = m[1].trim();
    if (p) out.push(p);
  }
  return out;
}

function isPreviewablePath(p: string): boolean {
  return PREVIEW_EXT.test(basename(p)) || PREVIEW_EXT.test(foldPath(p));
}

/** Products first; if this turn only attached a file, still open it in 结果区. */
export function turnPreviewPaths(events: SessionEvent[], workspace?: string): string[] {
  const products = turnArtifacts(events, workspace);
  if (products.length) return products;
  const seen = new Map<string, string>();
  for (const raw of userAttachedPaths(events)) {
    const p = foldPath(raw.replace(/^['"`]|['"`]$/g, ""));
    if (!p || !isPreviewablePath(p)) continue;
    if (isJunkPath(p) && !isUploadPath(p) && !isGeneratedPath(p)) continue;
    const shown = toDisplayPath(p, workspace);
    const key = foldPath(shown).toLowerCase();
    if (!seen.has(key)) seen.set(key, shown);
  }
  return [...seen.values()];
}
