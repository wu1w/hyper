// DEPS: docx-preview mammoth html-to-docx jszip
import { useEffect, useRef, useState } from "react";
import { renderAsync } from "docx-preview";
import JSZip from "jszip";
import * as mammothMod from "mammoth";
import type { PreviewProps } from "./kinds";
import { looksLikeOle, looksLikeZip, toArrayBuffer, toBlob } from "./bytes";

declare module "html-to-docx" {
  export default function HTMLtoDOCX(
    html: string,
    header?: string | null,
    options?: Record<string, unknown>,
    footer?: string | null,
  ): Promise<Blob | ArrayBuffer | Uint8Array>;
}

type ConvertToHtml = (
  input: { arrayBuffer: ArrayBuffer },
  options?: { convertImage?: unknown },
) => Promise<{ value: string }>;

function mammothApi(): { convertToHtml: ConvertToHtml; dataUri?: unknown } {
  const mod = mammothMod as unknown as {
    convertToHtml?: ConvertToHtml;
    images?: { dataUri?: unknown };
    default?: { convertToHtml?: ConvertToHtml; images?: { dataUri?: unknown } };
  };
  const api = mod.convertToHtml ? mod : mod.default;
  if (!api?.convertToHtml) throw new Error("mammoth 不可用");
  return { convertToHtml: api.convertToHtml, dataUri: api.images?.dataUri ?? mod.images?.dataUri };
}

function xmlTextFallback(xml: string): string {
  return xml
    .replace(/<w:tab\b[^>]*\/>/g, "\t")
    .replace(/<w:br\b[^>]*\/>/g, "\n")
    .replace(/<w:p[ >]/g, "\n")
    .replace(/<[^>]+>/g, "")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&amp;/g, "&")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

async function extractDocxBody(bytes: Uint8Array): Promise<string> {
  const zip = await JSZip.loadAsync(bytes);
  const f = zip.file("word/document.xml");
  if (!f) throw new Error("缺少 word/document.xml");
  const xml = await f.async("string");
  const text = xmlTextFallback(xml);
  if (!text) throw new Error("正文为空");
  return text
    .split("\n")
    .map((l) => l.trimEnd())
    .join("\n");
}

function htmlEscape(s: string): string {
  return s.replace(/[&<>]/g, (ch) => (ch === "&" ? "&amp;" : ch === "<" ? "&lt;" : "&gt;"));
}

function textAsHtml(text: string): string {
  return text
    .split("\n")
    .map((l) => `<p>${htmlEscape(l) || "<br>"}</p>`)
    .join("");
}

async function toUint8Array(out: unknown): Promise<Uint8Array> {
  if (out instanceof Uint8Array) return out;
  if (out instanceof ArrayBuffer) return new Uint8Array(out);
  if (typeof Blob !== "undefined" && out instanceof Blob) return new Uint8Array(await out.arrayBuffer());
  throw new Error("无法生成 docx");
}

function wrapHtmlDocument(body: string): string {
  return `<!DOCTYPE html><html><head><meta charset="UTF-8"></head><body>${body}</body></html>`;
}

function htmlToDocxFn(mod: unknown): (
  html: string,
  header?: string | null,
  options?: Record<string, unknown>,
  footer?: string | null,
) => Promise<unknown> {
  if (typeof mod === "function") return mod as ReturnType<typeof htmlToDocxFn>;
  if (mod && typeof mod === "object") {
    const rec = mod as { default?: unknown; HTMLtoDOCX?: unknown };
    const fn = rec.default ?? rec.HTMLtoDOCX;
    if (typeof fn === "function") return fn as ReturnType<typeof htmlToDocxFn>;
  }
  throw new Error("html-to-docx 不可用");
}

async function viaHtmlToDocx(html: string, title: string): Promise<Uint8Array> {
  const mod = await import("html-to-docx");
  const convert = htmlToDocxFn(mod);
  const out = await convert(wrapHtmlDocument(html), null, {
    title,
    lang: "zh-CN",
    font: "Microsoft YaHei",
    decodeUnicode: true,
  });
  const bytes = await toUint8Array(out);
  if (!looksLikeZip(bytes)) throw new Error("html-to-docx 输出无效");
  return bytes;
}

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(data: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < data.length; i++) c = CRC_TABLE[(c ^ data[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function concatBytes(parts: Uint8Array[]): Uint8Array {
  const n = parts.reduce((a, p) => a + p.length, 0);
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function zipStore(files: Array<{ name: string; data: Uint8Array }>): Uint8Array {
  const enc = new TextEncoder();
  const locals: Uint8Array[] = [];
  const centrals: Uint8Array[] = [];
  let offset = 0;
  const now = new Date();
  const dosTime =
    ((now.getHours() & 0x1f) << 11) | ((now.getMinutes() & 0x3f) << 5) | ((Math.floor(now.getSeconds() / 2) & 0x1f));
  const dosDate =
    (((now.getFullYear() - 1980) & 0x7f) << 9) | (((now.getMonth() + 1) & 0xf) << 5) | (now.getDate() & 0x1f);

  for (const file of files) {
    const name = enc.encode(file.name);
    const crc = crc32(file.data);
    const local = new Uint8Array(30 + name.length + file.data.length);
    const lv = new DataView(local.buffer);
    lv.setUint32(0, 0x04034b50, true);
    lv.setUint16(4, 20, true);
    lv.setUint16(6, 0x800, true);
    lv.setUint16(8, 0, true);
    lv.setUint16(10, dosTime, true);
    lv.setUint16(12, dosDate, true);
    lv.setUint32(14, crc, true);
    lv.setUint32(18, file.data.length, true);
    lv.setUint32(22, file.data.length, true);
    lv.setUint16(26, name.length, true);
    local.set(name, 30);
    local.set(file.data, 30 + name.length);
    locals.push(local);

    const central = new Uint8Array(46 + name.length);
    const cv = new DataView(central.buffer);
    cv.setUint32(0, 0x02014b50, true);
    cv.setUint16(4, 20, true);
    cv.setUint16(6, 20, true);
    cv.setUint16(8, 0x800, true);
    cv.setUint16(10, 0, true);
    cv.setUint16(12, dosTime, true);
    cv.setUint16(14, dosDate, true);
    cv.setUint32(16, crc, true);
    cv.setUint32(20, file.data.length, true);
    cv.setUint32(24, file.data.length, true);
    cv.setUint16(28, name.length, true);
    cv.setUint32(42, offset, true);
    central.set(name, 46);
    centrals.push(central);
    offset += local.length;
  }

  const centralDir = concatBytes(centrals);
  const eocd = new Uint8Array(22);
  const ev = new DataView(eocd.buffer);
  ev.setUint32(0, 0x06054b50, true);
  ev.setUint16(8, files.length, true);
  ev.setUint16(10, files.length, true);
  ev.setUint32(12, centralDir.length, true);
  ev.setUint32(16, offset, true);
  return concatBytes([...locals, centralDir, eocd]);
}

function xmlEscape(s: string): string {
  return s.replace(/[&<>]/g, (ch) => (ch === "&" ? "&amp;" : ch === "<" ? "&lt;" : "&gt;"));
}

type RunStyle = { bold?: boolean; italic?: boolean; underline?: boolean; hyper?: boolean };

function wText(text: string): string {
  if (!text) return "";
  return `<w:t xml:space="preserve">${xmlEscape(text)}</w:t>`;
}

function wRun(text: string, style: RunStyle = {}): string {
  if (!text) return "";
  const rPr: string[] = [];
  if (style.bold) rPr.push("<w:b/><w:bCs/>");
  if (style.italic) rPr.push("<w:i/><w:iCs/>");
  if (style.underline || style.hyper) rPr.push('<w:u w:val="single"/>');
  if (style.hyper) rPr.push('<w:color w:val="0563C1"/>');
  const pr = rPr.length ? `<w:rPr>${rPr.join("")}</w:rPr>` : "";
  return `<w:r>${pr}${wText(text)}</w:r>`;
}

function mergeStyle(base: RunStyle, extra: RunStyle): RunStyle {
  return {
    bold: base.bold || extra.bold,
    italic: base.italic || extra.italic,
    underline: base.underline || extra.underline,
    hyper: base.hyper || extra.hyper,
  };
}

function inlineXml(node: Node, style: RunStyle = {}): string {
  if (node.nodeType === Node.TEXT_NODE) return wRun(node.textContent || "", style);
  if (node.nodeType !== Node.ELEMENT_NODE) return "";
  const el = node as HTMLElement;
  const tag = el.tagName.toLowerCase();
  if (tag === "br") return "<w:r><w:br/></w:r>";
  if (tag === "script" || tag === "style") return "";
  const next = mergeStyle(style, {
    bold: tag === "strong" || tag === "b",
    italic: tag === "em" || tag === "i",
    underline: tag === "u",
    hyper: tag === "a",
  });
  let out = "";
  for (const child of Array.from(el.childNodes)) out += inlineXml(child, next);
  return out;
}

function wPara(inner: string, extraPr = ""): string {
  const pr = extraPr ? `<w:pPr>${extraPr}</w:pPr>` : "";
  return `<w:p>${pr}${inner || "<w:r/>"}</w:p>`;
}

const HEAD_SZ: Record<string, string> = {
  h1: "36",
  h2: "32",
  h3: "28",
  h4: "24",
  h5: "22",
  h6: "20",
};

function headingPr(tag: string): string {
  const sz = HEAD_SZ[tag];
  return `<w:rPr><w:b/><w:sz w:val="${sz}"/><w:szCs w:val="${sz}"/></w:rPr>`;
}

function blocksXml(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) {
    const t = (node.textContent || "").replace(/\s+/g, " ");
    return t.trim() ? wPara(wRun(t)) : "";
  }
  if (node.nodeType !== Node.ELEMENT_NODE) return "";
  const el = node as HTMLElement;
  const tag = el.tagName.toLowerCase();
  if (tag === "script" || tag === "style") return "";
  if (HEAD_SZ[tag]) return wPara(inlineXml(el, { bold: true }), headingPr(tag));
  if (tag === "p" || tag === "div" || tag === "blockquote" || tag === "pre" || tag === "figcaption") {
    const nested = Array.from(el.children).some((c) =>
      /^(P|DIV|H[1-6]|UL|OL|TABLE|BLOCKQUOTE|PRE)$/.test(c.tagName),
    );
    if (nested) {
      let out = "";
      for (const child of Array.from(el.childNodes)) out += blocksXml(child);
      return out;
    }
    return wPara(inlineXml(el));
  }
  if (tag === "li") return wPara(inlineXml(el), "<w:ind w:left=\"360\"/>");
  if (tag === "ul" || tag === "ol") {
    let out = "";
    for (const child of Array.from(el.childNodes)) out += blocksXml(child);
    return out;
  }
  if (tag === "br") return wPara("");
  if (tag === "table") return tableXml(el as HTMLTableElement);
  if (tag === "img") {
    const alt = el.getAttribute("alt") || "图";
    return wPara(wRun(`[${alt}]`));
  }
  let out = "";
  for (const child of Array.from(el.childNodes)) out += blocksXml(child);
  return out;
}

function tableXml(table: HTMLTableElement): string {
  const rows = Array.from(table.rows);
  if (!rows.length) return "";
  const cols = Math.max(1, ...rows.map((r) => r.cells.length));
  const cw = Math.floor(9000 / cols);
  const grid = `<w:tblGrid>${Array.from({ length: cols }, () => `<w:gridCol w:w="${cw}"/>`).join("")}</w:tblGrid>`;
  const body = rows
    .map((row) => {
      const cells = Array.from(row.cells)
        .map((cell) => {
          const inner = Array.from(cell.childNodes).map(blocksXml).join("") || wPara("");
          return `<w:tc><w:tcPr><w:tcW w:w="${cw}" w:type="dxa"/></w:tcPr>${inner}</w:tc>`;
        })
        .join("");
      return `<w:tr>${cells}</w:tr>`;
    })
    .join("");
  return `<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/><w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:left w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:right w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="auto"/></w:tblBorders></w:tblPr>${grid}${body}</w:tbl>`;
}

function htmlToDocumentXml(html: string): string {
  const doc = new DOMParser().parseFromString(`<div id="root">${html}</div>`, "text/html");
  const root = doc.getElementById("root") || doc.body;
  let body = "";
  for (const child of Array.from(root.childNodes)) body += blocksXml(child);
  if (!body.trim()) body = wPara("");
  return `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>${body}<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr></w:body></w:document>`;
}

function fallbackDocx(html: string): Uint8Array {
  const enc = new TextEncoder();
  return zipStore([
    {
      name: "[Content_Types].xml",
      data: enc.encode(`<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>`),
    },
    {
      name: "_rels/.rels",
      data: enc.encode(`<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>`),
    },
    {
      name: "word/_rels/document.xml.rels",
      data: enc.encode(`<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>`),
    },
    { name: "word/document.xml", data: enc.encode(htmlToDocumentXml(html)) },
  ]);
}

async function htmlToDocxBytes(html: string, title: string): Promise<Uint8Array> {
  try {
    return await viaHtmlToDocx(html, title);
  } catch {
    return fallbackDocx(html);
  }
}

function legacyWordNote(path: string, data: Uint8Array): string | null {
  const n = path.toLowerCase();
  if (n.endsWith(".doc") && !n.endsWith(".docx") && !n.endsWith(".docm")) {
    return "这是旧版 .doc（OLE 复合文档），浏览器无法排版。请用 Word / WPS 另存为 .docx 后再预览。";
  }
  if (n.endsWith(".odt")) {
    return "OpenDocument（.odt）暂不能按 Word 版式预览。请另存为 .docx。";
  }
  if (looksLikeOle(data)) {
    return "文件是旧版 Office 二进制，不是 docx 压缩包。请另存为 .docx。";
  }
  if (!looksLikeZip(data)) {
    return "不是有效的 docx 压缩包。";
  }
  return null;
}

export function WordPreview({ path, bytes, url, onDirty, registerExport }: PreviewProps) {
  const previewRef = useRef<HTMLDivElement>(null);
  const editorRef = useRef<HTMLDivElement>(null);
  const origRef = useRef<Uint8Array>(bytes.slice());
  const initialHtmlRef = useRef("");
  const [loading, setLoading] = useState(true);
  const [previewOk, setPreviewOk] = useState(false);
  const [canEdit, setCanEdit] = useState(false);
  const [note, setNote] = useState("正在打开文档…");
  const legacy = legacyWordNote(path, bytes);

  useEffect(() => {
    origRef.current = bytes.slice();
  }, [bytes]);

  useEffect(() => {
    registerExport(async () => {
      if (legacy) return origRef.current;
      const html = editorRef.current?.innerHTML ?? "";
      if (html === initialHtmlRef.current) return origRef.current;
      return htmlToDocxBytes(html, path.split(/[\\/]/).pop() || path);
    });
  }, [path, registerExport, legacy]);

  useEffect(() => {
    let gone = false;
    const copy = bytes.slice();
    origRef.current = copy;
    const preview = previewRef.current;
    const editor = editorRef.current;
    setLoading(true);
    setPreviewOk(false);
    setCanEdit(false);
    setNote("正在打开文档…");
    if (preview) preview.innerHTML = "";
    if (editor) editor.innerHTML = "";
    onDirty(false);

    const blocked = legacyWordNote(path, copy);
    if (blocked) {
      setNote(blocked);
      setLoading(false);
      return () => {
        gone = true;
      };
    }

    (async () => {
      let html = "";
      try {
        const api = mammothApi();
        const result = await api.convertToHtml(
          { arrayBuffer: toArrayBuffer(copy) },
          api.dataUri ? { convertImage: api.dataUri } : undefined,
        );
        if (result.value.trim()) html = result.value;
      } catch {
        /* zip text next */
      }
      if (!html) {
        try {
          html = textAsHtml(await extractDocxBody(copy));
        } catch {
          html = "";
        }
      }
      if (gone) return;

      if (editor) {
        editor.innerHTML = html || "<p></p>";
        initialHtmlRef.current = editor.innerHTML;
      } else {
        initialHtmlRef.current = html;
      }
      setCanEdit(Boolean(html));

      let layoutOk = false;
      if (preview) {
        try {
          await renderAsync(
            toBlob(copy, "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            preview,
            undefined,
            {
              className: "pv-word-doc",
              inWrapper: true,
              ignoreWidth: false,
              ignoreHeight: false,
              breakPages: true,
              useBase64URL: true,
              renderChanges: true,
              renderHeaders: true,
              renderFooters: true,
              renderFootnotes: true,
              experimental: true,
            },
          );
          layoutOk = preview.childNodes.length > 0;
        } catch {
          preview.innerHTML = "";
          layoutOk = false;
        }
      }
      if (gone) return;
      setPreviewOk(layoutOk);
      setNote(
        layoutOk
          ? html
            ? "版式预览；改正文后保存会重建 docx。"
            : "版式预览（正文抽取失败，保存仍写回原文件）。"
          : html
            ? "版式预览失败，已显示正文。保存会按正文重建 docx。"
            : "无法打开这份 Word。可下载后用桌面 Word / WPS 查看。",
      );
      setLoading(false);
    })();

    return () => {
      gone = true;
    };
  }, [bytes, onDirty, path]);

  const onInput = () => {
    const html = editorRef.current?.innerHTML ?? "";
    onDirty(html !== initialHtmlRef.current);
  };

  return (
    <div className="pv-fallback pv-word">
      <p className="sub">{note}</p>
      <div className="pv-word-stage" hidden={!!legacy || (!loading && !previewOk)} data-src={url}>
        <div className="pv-word-doc" ref={previewRef} />
      </div>
      {legacy ? null : (
        <details className="pv-ppt-edit" open={!previewOk && !loading && canEdit}>
          <summary>改正文</summary>
          <div
            className="pv-text pv-word-editor"
            contentEditable={!loading && canEdit}
            suppressContentEditableWarning
            spellCheck={false}
            role="textbox"
            aria-multiline="true"
            aria-label="Word 正文"
            ref={editorRef}
            onInput={onInput}
          />
        </details>
      )}
    </div>
  );
}
