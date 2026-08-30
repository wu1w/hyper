export const esc = (s: string) =>
  s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]!));

export type Align = "l" | "c" | "r";
export type ListItem = { html: string; task?: boolean; checked?: boolean };

export type Block =
  | { k: "p"; html: string }
  | { k: "h"; lvl: number; html: string }
  | { k: "list"; ordered: boolean; items: ListItem[] }
  | { k: "quote"; html: string }
  | { k: "hr" }
  | { k: "code"; lang: string; code: string }
  | { k: "table"; align: Align[]; head: string[]; rows: string[][] };

/** 行内：转义后再套标记。图片允许 http(s)、本机文件接口、工作区相对路径。 */
export function safeImgSrc(raw: string): string | null {
  let u = raw.trim().replace(/&amp;/g, "&");
  if (!u || u.length > 4096) return null;
  if (u.startsWith("//")) return null;
  if (/^https:\/\//i.test(u)) return u;
  if (/^http:\/\/(127\.0\.0\.1|localhost|\[::1\])(?::\d+)?\//i.test(u)) return u;
  if (/^data:image\/(?:png|jpe?g|gif|webp|svg\+xml);base64,/i.test(u)) return u;
  if (/^\/api\/files\?/i.test(u)) return u;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(u)) return null;
  if (u.includes("..")) return null;
  u = u.replace(/^\.\//, "");
  if (!u) return null;
  return `/api/files?path=${encodeURIComponent(u)}`;
}

export function mdInline(escaped: string): string {
  let s = escaped.replace(/`([^`]+)`/g, "<code>$1</code>");
  s = s.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (_m, alt: string, src: string) => {
    const safe = safeImgSrc(src);
    if (!safe) return _m;
    return `<img class="md-img" alt="${alt}" src="${esc(safe)}" loading="lazy" />`;
  });
  s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>');
  s = s.replace(
    /\[([^\]]+)\]\(([0-9a-f]{8,32}-[0-9a-f]{8})\)/gi,
    '<button type="button" class="md-agent" data-agent-id="$2">$1</button>',
  );
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/~~([^~]+)~~/g, "<del>$1</del>");
  s = s.replace(/(^|[^*])\*([^*\n]+)\*(?!\*)/g, "$1<em>$2</em>");
  s = s.replace(/(^|[\s(（【])_([^_\n]+)_(?=[\s).,!?;:）】]|$)/g, "$1<em>$2</em>");
  s = s.replace(
    /(^|[^"'>])(https?:\/\/[^\s<]+[^\s<.,;:!?]) /g,
    '$1<a href="$2" target="_blank" rel="noreferrer">$2</a>',
  );
  // 行尾裸链（上面那条要求尾空格）
  s = s.replace(
    /(^|[^"'>])(https?:\/\/[^\s<]+[^\s<.,;:!?])$/g,
    '$1<a href="$2" target="_blank" rel="noreferrer">$2</a>',
  );
  return s;
}

export function inline(raw: string): string {
  return mdInline(esc(raw));
}

export function parseMd(src: string): Block[] {
  const lines = src.replace(/\r\n/g, "\n").split("\n");
  const out: Block[] = [];
  let i = 0;
  let para: string[] = [];
  let list: { ordered: boolean; items: ListItem[] } | null = null;
  let quote: string[] = [];

  const flushPara = () => {
    if (para.length) {
      out.push({ k: "p", html: para.join("<br>") });
      para = [];
    }
  };
  const flushList = () => {
    if (list) {
      out.push({ k: "list", ordered: list.ordered, items: list.items });
      list = null;
    }
  };
  const flushQuote = () => {
    if (quote.length) {
      out.push({ k: "quote", html: quote.join("<br>") });
      quote = [];
    }
  };
  const flushText = () => {
    flushPara();
    flushList();
    flushQuote();
  };

  while (i < lines.length) {
    const raw = lines[i];
    const t = raw.trim();

    const fence = /^(`{3,})(.*)$/.exec(t);
    if (fence) {
      flushText();
      const ticks = fence[1].length;
      const lang = fence[2].trim().split(/\s+/)[0] || "";
      const body: string[] = [];
      i++;
      while (i < lines.length) {
        const close = lines[i].trim();
        const closer = /^(`{3,})\s*[\w+-]*\s*$/.exec(close);
        if (closer && closer[1].length >= ticks) break;
        body.push(lines[i]);
        i++;
      }
      if (i < lines.length) i++;
      out.push({ k: "code", lang, code: body.join("\n").replace(/\n$/, "") });
      continue;
    }

    if (!t) {
      flushText();
      i++;
      continue;
    }

    if (/^(-{3,}|\*{3,}|_{3,})$/.test(t)) {
      flushText();
      out.push({ k: "hr" });
      i++;
      continue;
    }

    const tbl = readTable(lines, i);
    if (tbl) {
      flushText();
      out.push(tbl.block);
      i = tbl.next;
      continue;
    }

    const h = /^(#{1,4})\s+(.*)$/.exec(t);
    if (h) {
      flushText();
      const lvl = Math.min(6, h[1].length + 2);
      out.push({ k: "h", lvl, html: inline(h[2]) });
      i++;
      continue;
    }

    const q = /^>\s?(.*)$/.exec(t);
    if (q) {
      flushPara();
      flushList();
      quote.push(inline(q[1]));
      i++;
      continue;
    }
    if (quote.length) flushQuote();

    const task = /^[-*•]\s+\[([ xX])\]\s+(.*)$/.exec(t);
    if (task) {
      flushPara();
      if (list?.ordered) flushList();
      if (!list) list = { ordered: false, items: [] };
      list.items.push({ html: inline(task[2]), task: true, checked: task[1] !== " " });
      i++;
      continue;
    }

    const ul = /^[-*•]\s+(.*)$/.exec(t);
    if (ul) {
      flushPara();
      if (list?.ordered) flushList();
      if (!list) list = { ordered: false, items: [] };
      list.items.push({ html: inline(ul[1]) });
      i++;
      continue;
    }

    const ol = /^\d{1,3}[.、)]\s+(.*)$/.exec(t);
    if (ol) {
      flushPara();
      if (list && !list.ordered) flushList();
      if (!list) list = { ordered: true, items: [] };
      list.items.push({ html: inline(ol[1]) });
      i++;
      continue;
    }

    flushList();
    para.push(inline(raw));
    i++;
  }
  flushText();
  return out;
}

/** Index after the last blank line that is not inside a fence. Live markdown
 * only reparses the open tail so a long coding reply does not parseMd the
 * whole buffer on every token. */
export function lastStableMdCut(src: string): number {
  const s = src.replace(/\r\n/g, "\n");
  let fence: number | null = null;
  let lastCut = 0;
  let i = 0;
  while (i <= s.length) {
    const nl = s.indexOf("\n", i);
    const end = nl < 0 ? s.length : nl;
    const line = s.slice(i, end).trim();
    const ticks = /^(`{3,})/.exec(line);
    if (ticks) {
      const n = ticks[1].length;
      if (fence == null) fence = n;
      else if (n >= fence) fence = null;
    } else if (fence == null && !line) {
      lastCut = nl < 0 ? s.length : nl + 1;
    }
    if (nl < 0) break;
    i = nl + 1;
  }
  return lastCut;
}

export type MdParseCache = { prefix: string; blocks: Block[] };

export function parseMdCached(src: string, cache: MdParseCache): Block[] {
  const s = src.replace(/\r\n/g, "\n");
  const cut = lastStableMdCut(s);
  const prefix = s.slice(0, cut);
  if (prefix !== cache.prefix) {
    cache.prefix = prefix;
    cache.blocks = prefix ? parseMd(prefix) : [];
  }
  const tail = s.slice(cut);
  if (!tail) return cache.blocks;
  return cache.blocks.concat(parseMd(tail));
}

function readTable(lines: string[], i: number): { block: Extract<Block, { k: "table" }>; next: number } | null {
  if (i + 1 >= lines.length) return null;
  if (lines[i].indexOf("|") < 0) return null;
  const head = splitRow(lines[i]);
  const sep = splitRow(lines[i + 1]);
  if (!head.length || sep.length < head.length) return null;
  if (!sep.every((c) => /^:?-{2,}:?$/.test(c))) return null;
  const align: Align[] = sep.map((c) => {
    const l = c.startsWith(":");
    const r = c.endsWith(":");
    return l && r ? "c" : r ? "r" : "l";
  });
  const rows: string[][] = [];
  let j = i + 2;
  while (j < lines.length && lines[j].indexOf("|") >= 0 && lines[j].trim()) {
    if (/^[-*•>\s#`]/.test(lines[j].trim()) && !lines[j].includes("|")) break;
    rows.push(padRow(splitRow(lines[j]), head.length));
    j++;
  }
  return { block: { k: "table", align, head: padRow(head, head.length), rows }, next: j };
}

function splitRow(line: string): string[] {
  let s = line.trim();
  if (s.startsWith("|")) s = s.slice(1);
  if (s.endsWith("|")) s = s.slice(0, -1);
  return s.split("|").map((c) => c.trim());
}
function padRow(row: string[], n: number): string[] {
  const out = row.slice(0, n);
  while (out.length < n) out.push("");
  return out;
}

