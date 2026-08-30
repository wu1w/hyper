import { memo, useMemo, useRef, useState } from "react";
import { asChartJson, DiagramView, parseChart, parseMermaid, type Diagram } from "./md-chart";
import { highlight, normLang, type HlTok } from "./md-hl";
import {
  type Align,
  type Block,
  type MdParseCache,
  esc,
  inline,
  parseMd,
  parseMdCached,
} from "./md-parse";
import { Icon } from "./ui";

export {
  lastStableMdCut,
  mdInline,
  parseMd,
  parseMdCached,
  safeImgSrc,
  type MdParseCache,
} from "./md-parse";

function looksMermaid(lang: string, code: string): boolean {
  const l = normLang(lang);
  if (l === "mermaid" || l === "mmd") return true;
  if (l) return false;
  return /^(?:graph|flowchart|sequenceDiagram|pie|xychart(?:-beta)?)\b/.test(code.trim());
}

function diagramOf(lang: string, code: string, live?: boolean): Diagram | null {
  if (live) return null;
  const l = normLang(lang);
  if (l === "chart") return parseChart(code);
  if (l === "json") {
    const j = asChartJson(code);
    return j ? parseChart(code) : null;
  }
  if (looksMermaid(lang, code)) return parseMermaid(code);
  return null;
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

function sanitizeSvg(src: string): string | null {
  const t = src.trim();
  if (!/^<svg[\s>]/i.test(t)) return null;
  try {
    const doc = new DOMParser().parseFromString(t, "image/svg+xml");
    if (doc.querySelector("parsererror")) return null;
    const svg = doc.documentElement;
    if (svg.tagName.toLowerCase() !== "svg") return null;
    const bad = new Set(["script", "foreignobject", "iframe", "object", "embed", "animate", "set", "animatetransform", "animatecolor", "animatemotion"]);
    const walk = (el: Element) => {
      const kids = [...el.children];
      for (const kid of kids) {
        if (bad.has(kid.tagName.toLowerCase())) {
          kid.remove();
          continue;
        }
        for (const attr of [...kid.attributes]) {
          const n = attr.name.toLowerCase();
          const v = attr.value.trim();
          if (n.startsWith("on") || n === "href" || n.endsWith(":href")) {
            if (n.startsWith("on") || /^(javascript|data):/i.test(v)) kid.removeAttribute(attr.name);
          }
        }
        walk(kid);
      }
    };
    walk(svg);
    for (const attr of [...svg.attributes]) {
      if (attr.name.toLowerCase().startsWith("on")) svg.removeAttribute(attr.name);
    }
    return new XMLSerializer().serializeToString(svg);
  } catch {
    return null;
  }
}

function CodeBlock({ lang, code, live }: { lang: string; code: string; live?: boolean }) {
  const [copied, setCopied] = useState(false);
  const [src, setSrc] = useState(false);
  const diagram = useMemo(() => diagramOf(lang, code, live), [lang, code, live]);
  const svg = useMemo(() => (normLang(lang) === "svg" && !live ? sanitizeSvg(code) : null), [lang, code, live]);
  const showFig = (!!diagram || !!svg) && !src;
  const label = diagram ? (diagram.kind === "pie" ? "pie" : diagram.kind === "xy" ? "chart" : diagram.kind === "seq" ? "sequence" : "mermaid") : lang || "code";
  const toks = useMemo(
    () => (live || showFig ? [] : highlight(code, lang)),
    [live, showFig, code, lang],
  );

  const onCopy = () => {
    void copyText(code).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    });
  };

  return (
    <div className={`md-code${showFig ? " fig" : ""}`}>
      <div className="md-code-bar">
        <span className="md-code-lang">{label}</span>
        <span className="md-code-actions">
          {diagram || svg ? (
            <button type="button" className="md-code-btn" onClick={() => setSrc((v) => !v)}>
              {src ? "图" : "源码"}
            </button>
          ) : null}
          <button type="button" className="md-code-btn" onClick={onCopy} aria-label="复制代码">
            <Icon name={copied ? "check" : "copy"} />
            {copied ? "已复制" : "复制"}
          </button>
        </span>
      </div>
      {showFig && diagram ? <DiagramView d={diagram} /> : null}
      {showFig && svg ? (
        <div className="md-svg" dangerouslySetInnerHTML={{ __html: svg }} />
      ) : null}
      {!showFig ? (
        <pre>
          <code>
            {toks.length ? <Hl toks={toks} /> : code}
          </code>
        </pre>
      ) : null}
    </div>
  );
}

function Hl({ toks }: { toks: HlTok[] }) {
  return (
    <>
      {toks.map((t, i) =>
        t.k ? (
          <span key={i} className={`hl-${t.k}`}>
            {t.t}
          </span>
        ) : (
          <span key={i}>{t.t}</span>
        ),
      )}
    </>
  );
}

function isNumeric(s: string) {
  const t = s.replace(/,/g, "").trim();
  if (!t) return false;
  return /^-?\d+(\.\d+)?%?$/.test(t) || /^-?\d+(\.\d+)?[eE][+-]?\d+$/.test(t);
}
function numVal(s: string) {
  return Number(s.replace(/,/g, "").replace(/%$/, "").trim());
}

function TableBlock({ align, head, rows }: { align: Align[]; head: string[]; rows: string[][] }) {
  const numeric = head.map((_, c) => rows.length > 0 && rows.every((r) => !r[c] || isNumeric(r[c])));
  const maxes = numeric.map((on, c) => (on ? Math.max(0, ...rows.map((r) => Math.abs(numVal(r[c] || "0")))) : 0));
  return (
    <div className="md-table-wrap">
      <table className="md-table">
        <thead>
          <tr>
            {head.map((h, i) => (
              <th key={i} className={align[i] === "c" ? "c" : align[i] === "r" || numeric[i] ? "r" : undefined}>
                <span dangerouslySetInnerHTML={{ __html: inline(h) }} />
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, ri) => (
            <tr key={ri}>
              {row.map((cell, ci) => {
                const n = numeric[ci];
                const v = n ? numVal(cell) : 0;
                const pct = n && maxes[ci] > 0 ? Math.min(100, (Math.abs(v) / maxes[ci]) * 100) : 0;
                return (
                  <td
                    key={ci}
                    className={`${align[ci] === "c" ? "c" : align[ci] === "r" || n ? "r" : ""}${n ? " num" : ""}`}
                  >
                    {n && pct > 0 ? <span className="md-spark" style={{ width: `${pct}%` }} /> : null}
                    <span dangerouslySetInnerHTML={{ __html: n ? esc(cell) : inline(cell) }} />
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function BlockView({ b, live }: { b: Block; live?: boolean }) {
  switch (b.k) {
    case "p":
      return <p dangerouslySetInnerHTML={{ __html: b.html }} />;
    case "h": {
      const Tag = `h${b.lvl}` as "h3" | "h4" | "h5" | "h6";
      return <Tag dangerouslySetInnerHTML={{ __html: b.html }} />;
    }
    case "quote":
      return <blockquote dangerouslySetInnerHTML={{ __html: b.html }} />;
    case "hr":
      return <hr className="md-hr" />;
    case "code":
      return <CodeBlock lang={b.lang} code={b.code} live={live} />;
    case "table":
      return <TableBlock align={b.align} head={b.head} rows={b.rows} />;
    case "list": {
      const Tag = b.ordered ? "ol" : "ul";
      return (
        <Tag className={b.items.some((it) => it.task) ? "md-tasks" : undefined}>
          {b.items.map((it, i) => (
            <li key={i} className={it.task ? "md-task" : undefined}>
              {it.task ? (
                <input type="checkbox" disabled checked={!!it.checked} aria-hidden />
              ) : null}
              <span dangerouslySetInnerHTML={{ __html: it.html }} />
            </li>
          ))}
        </Tag>
      );
    }
    default:
      return null;
  }
}

/** 正文块：parse 只在文本变化时重算。流式未闭合的围栏当代码块，图等收束后再画。 */
export const MdText = memo(function MdText({
  text,
  live,
  onOpenAgent,
}: {
  text: string;
  live?: boolean;
  onOpenAgent?: (id: string, label: string, status?: string, type?: string, detail?: string) => void;
}) {
  const cache = useRef<MdParseCache>({ prefix: "", blocks: [] });
  const blocks = useMemo(() => {
    if (!live) {
      cache.current = { prefix: "", blocks: [] };
      return parseMd(text);
    }
    return parseMdCached(text, cache.current);
  }, [text, live]);
  return (
    <div
      className={`msg-a${live ? " caret" : ""}`}
      onClick={(e) => {
        if (!onOpenAgent) return;
        const el = (e.target as HTMLElement | null)?.closest?.("[data-agent-id]");
        if (!el) return;
        e.preventDefault();
        const id = el.getAttribute("data-agent-id") || "";
        if (id) onOpenAgent(id, (el.textContent || "").trim() || id);
      }}
    >
      {blocks.map((b, i) => (
        <BlockView key={i} b={b} live={live && i === blocks.length - 1} />
      ))}
    </div>
  );
});

