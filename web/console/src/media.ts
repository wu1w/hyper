/** Workspace / uploaded media for the console. Backend `/api/files` is the gate. */

import type { Uploaded } from "./api";

export type StoredMedia = {
  kind: string;
  mime: string;
  url: string;
};

const IMAGE_EXT = /\.(png|jpe?g|gif|webp|svg|bmp)$/i;

export function isImagePath(path: string): boolean {
  return IMAGE_EXT.test(path.split(/[?#]/)[0] || "");
}

export function isImageMedia(m: { kind?: string; mime?: string; url?: string }): boolean {
  if ((m.kind || "").toLowerCase() === "image") return true;
  if ((m.mime || "").toLowerCase().startsWith("image/")) return true;
  return isImagePath(m.url || "");
}

/** Resolve a stored url (data / http / /api/files / workspace path) for <img src>. */
export function mediaSrc(url: string): string {
  const u = url.trim();
  if (!u) return "";
  if (/^(data:|https?:\/\/|\/api\/)/i.test(u)) return u;
  return `/api/files?path=${encodeURIComponent(u)}`;
}

export function fileHref(path: string, download = false): string {
  const u = `/api/files?path=${encodeURIComponent(path)}`;
  return download ? `${u}&dl=1` : u;
}

/** Workspace-relative path from a media url, or "" if it is a data/http URL. */
export function pathFromMediaUrl(url: string): string {
  const u = url.trim();
  if (!u || /^(data:|https?:\/\/)/i.test(u)) return "";
  if (u.startsWith("/api/files")) {
    try {
      return new URL(u, "http://local").searchParams.get("path") || "";
    } catch {
      return "";
    }
  }
  return u;
}

export function downloadHref(src: string): string {
  if (src.startsWith("data:")) return src;
  if (src.startsWith("/api/files")) {
    return /(?:\?|&)dl=/.test(src) ? src : `${src}${src.includes("?") ? "&" : "?"}dl=1`;
  }
  return src;
}

export function parseStoredMedia(raw: unknown): StoredMedia[] {
  if (!Array.isArray(raw)) return [];
  const out: StoredMedia[] = [];
  for (const item of raw) {
    if (!item || typeof item !== "object") continue;
    const o = item as Record<string, unknown>;
    const url = String(o.url || o.image_url || "").trim();
    if (!url) continue;
    out.push({
      kind: String(o.kind || o.type || ""),
      mime: String(o.mime || ""),
      url,
    });
  }
  return out;
}

export function stripAttachedNotes(text: string): string {
  return text
    .replace(/(?:^|\n)\[attached: [^\]]+\]/g, "")
    .replace(/^\s+$/m, "")
    .trim();
}

export function attachedPaths(text: string): string[] {
  const out: string[] = [];
  const re = /\[attached: ([^\]]+)\]/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    const p = m[1].trim();
    if (p) out.push(p);
  }
  return out;
}

/** User bubble: persisted `media` plus `[attached: path]` file chips. */
export function userMediaFromEvent(e: { media?: unknown; text?: string }): StoredMedia[] {
  const listed = parseStoredMedia(e.media);
  const seen = new Set(listed.map((m) => m.url));
  for (const p of attachedPaths(e.text || "")) {
    if (seen.has(p)) continue;
    seen.add(p);
    listed.push({
      kind: isImagePath(p) ? "image" : "file",
      mime: "",
      url: p,
    });
  }
  return listed;
}

export function filesFromClipboard(data: DataTransfer | null): File[] {
  if (!data) return [];
  const out: File[] = [];
  const seen = new Set<string>();
  const add = (f: File) => {
    if (f.size <= 0) return;
    const k = `${f.name}:${f.size}:${f.lastModified}`;
    if (seen.has(k)) return;
    seen.add(k);
    out.push(f);
  };
  for (const f of data.files) add(f);
  for (const it of data.items) {
    if (it.kind !== "file") continue;
    const f = it.getAsFile();
    if (f) add(f);
  }
  return out;
}

const SKIP_SRC = /pixel|tracking|spacer|1x1|analytics|doubleclick|facebook\.com\/tr|googletag/i;

export type ClipboardPaste = {
  files: File[];
  urls: string[];
  insertText: string;
};

export function clipboardPaste(data: DataTransfer | null): ClipboardPaste {
  const files = filesFromClipboard(data);
  const html = data?.getData("text/html") || "";
  const plain = (data?.getData("text/plain") || "").replace(/\u00a0/g, " ");
  const uriList = data?.getData("text/uri-list") || data?.getData("text/x-moz-url") || "";
  const htmlSrcs = files.length ? [] : htmlImageSrcs(html);
  const listSrcs = files.length ? [] : uriListSrcs(uriList);
  const urls: string[] = [];
  const seen = new Set<string>();
  for (const src of [...htmlSrcs, ...listSrcs]) {
    const u = normalizeClipSrc(src);
    if (!u || seen.has(u)) continue;
    seen.add(u);
    urls.push(u);
  }
  const insertText = pasteInsertText(plain, urls, files);
  return { files, urls, insertText };
}

export function htmlImageSrcs(html: string): string[] {
  if (!html.trim()) return [];
  const out: string[] = [];
  try {
    const doc = new DOMParser().parseFromString(html, "text/html");
    const add = (raw: string, el?: Element) => {
      const src = raw.trim();
      if (!src || out.includes(src)) return;
      if (!keepClipSrc(src, el)) return;
      out.push(src);
    };
    doc.querySelectorAll("img").forEach((img) => {
      add(
        img.getAttribute("src") ||
          img.getAttribute("data-src") ||
          img.getAttribute("data-original") ||
          img.getAttribute("data-lazy-src") ||
          "",
        img,
      );
      const set = img.getAttribute("srcset") || img.getAttribute("data-srcset") || "";
      if (set && !(img.getAttribute("src") || img.getAttribute("data-src") || img.getAttribute("data-original"))) {
        add(firstSrcset(set), img);
      }
    });
    doc.querySelectorAll("source[srcset]").forEach((el) => add(firstSrcset(el.getAttribute("srcset") || ""), el));
  } catch {
    const re = /<img\b[^>]*\b(?:src|data-src)=["']([^"']+)["'][^>]*>/gi;
    let m: RegExpExecArray | null;
    while ((m = re.exec(html))) {
      if (keepClipSrc(m[1]) && !out.includes(m[1])) out.push(m[1]);
    }
  }
  return out.slice(0, 8);
}

function uriListSrcs(raw: string): string[] {
  if (!raw.trim()) return [];
  const out: string[] = [];
  for (const line of raw.split(/\r?\n/)) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    const url = t.split("\t")[0]?.trim() || "";
    if (keepClipSrc(url) && isLikelyImageUrl(url) && !out.includes(url)) out.push(url);
  }
  return out.slice(0, 8);
}

function firstSrcset(s: string): string {
  return s.split(",")[0]?.trim().split(/\s+/)[0] || "";
}

function keepClipSrc(src: string, el?: Element): boolean {
  const u = src.trim();
  if (!u) return false;
  if (/^(cid:|blob:|file:|javascript:|about:)/i.test(u)) return false;
  if (SKIP_SRC.test(u)) return false;
  if (el) {
    const w = Number(el.getAttribute("width") || 0);
    const h = Number(el.getAttribute("height") || 0);
    if ((w > 0 && w <= 4) || (h > 0 && h <= 4)) return false;
  }
  if (u.startsWith("data:image/")) return u.length > 80;
  if (u.startsWith("data:")) return false;
  if (/^https?:\/\//i.test(u) || u.startsWith("//")) {
    if (el && el.tagName.toLowerCase() === "img") return true;
    return isLikelyImageUrl(u) || !/\.[a-z0-9]{1,5}$/i.test(u.split("?")[0] || "");
  }
  return false;
}

function isLikelyImageUrl(src: string): boolean {
  const path = src.split("?")[0] || "";
  if (IMAGE_EXT.test(path)) return true;
  if (/\/media\/|\/images?\/|\/img\/|twimg\.com|imgur\.com|ggpht\.com|googleusercontent\.com/i.test(src)) {
    return true;
  }
  return false;
}

function normalizeClipSrc(src: string): string {
  const u = src.trim();
  if (u.startsWith("//")) return `https:${u}`;
  return u;
}

function pasteInsertText(plain: string, urls: string[], files: File[]): string {
  const t = plain.trim();
  if (!t) return "";
  const tokens = t.split(/\s+/);
  const names = new Set(files.map((f) => f.name));
  const isUrl = (tok: string) => /^(https?:\/\/|data:image\/)\S+$/i.test(tok);
  if (
    tokens.length &&
    tokens.every(
      (tok) =>
        names.has(tok) ||
        isUrl(tok) ||
        urls.some((u) => tok === u || u.startsWith(tok) || tok.startsWith(u)),
    )
  ) {
    return "";
  }
  return plain;
}

export function fileFromDataUri(uri: string, name = "image.png"): File | null {
  const m = /^data:(image\/[a-zA-Z0-9.+-]+);base64,(.+)$/i.exec(uri.trim());
  if (!m) return null;
  try {
    const mime = m[1];
    const bin = atob(m[2].replace(/\s/g, ""));
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const ext = mime.split("/")[1]?.replace("+xml", "") || "png";
    const base = name.includes(".") ? name : `${name}.${ext}`;
    return new File([bytes], base, { type: mime });
  } catch {
    return null;
  }
}

export async function ingestRemoteImages(urls: string[]): Promise<Uploaded[]> {
  const out: Uploaded[] = [];
  for (const url of urls) {
    if (!/^https?:\/\//i.test(url)) continue;
    try {
      const r = await fetch("/api/ingest", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ url }),
      });
      if (!r.ok) continue;
      const j = (await r.json()) as { files?: Uploaded[] };
      if (j.files?.length) out.push(...j.files);
    } catch {
      /* skip this url */
    }
  }
  return out;
}

export function basename(path: string): string {
  const n = path.replace(/\\/g, "/").replace(/\/+$/, "");
  const i = n.lastIndexOf("/");
  return i >= 0 ? n.slice(i + 1) : n;
}

export function nameFromSrc(src: string): string {
  try {
    const u = new URL(src, "http://local");
    const p = u.searchParams.get("path");
    if (p) return basename(p) || "download";
  } catch {
    /* ignore */
  }
  if (src.startsWith("data:")) return "image";
  return basename(src.split("?")[0] || "download") || "download";
}
