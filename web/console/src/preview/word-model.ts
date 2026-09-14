import { looksLikeOle, looksLikeZip } from "./bytes.ts";

export const HEAD_SZ: Record<string, string> = {
  h1: "36",
  h2: "32",
  h3: "28",
  h4: "24",
  h5: "22",
  h6: "20",
};

export type RunStyle = { bold?: boolean; italic?: boolean; underline?: boolean; hyper?: boolean };

export function xmlEscape(s: string): string {
  return s.replace(/[&<>]/g, (ch) => (ch === "&" ? "&amp;" : ch === "<" ? "&lt;" : "&gt;"));
}

export function htmlEscape(s: string): string {
  return xmlEscape(s);
}

export function xmlTextFallback(xml: string): string {
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

export function textAsHtml(text: string): string {
  return text
    .split("\n")
    .map((l) => `<p>${htmlEscape(l) || "<br>"}</p>`)
    .join("");
}

export function wrapHtmlDocument(body: string): string {
  return `<!DOCTYPE html><html><head><meta charset="UTF-8"></head><body>${body}</body></html>`;
}

export function wText(text: string): string {
  if (!text) return "";
  return `<w:t xml:space="preserve">${xmlEscape(text)}</w:t>`;
}

export function wRun(text: string, style: RunStyle = {}): string {
  if (!text) return "";
  const rPr: string[] = [];
  if (style.bold) rPr.push("<w:b/><w:bCs/>");
  if (style.italic) rPr.push("<w:i/><w:iCs/>");
  if (style.underline || style.hyper) rPr.push('<w:u w:val="single"/>');
  if (style.hyper) rPr.push('<w:color w:val="0563C1"/>');
  const pr = rPr.length ? `<w:rPr>${rPr.join("")}</w:rPr>` : "";
  return `<w:r>${pr}${wText(text)}</w:r>`;
}

export function mergeRunStyle(base: RunStyle, extra: RunStyle): RunStyle {
  return {
    bold: base.bold || extra.bold,
    italic: base.italic || extra.italic,
    underline: base.underline || extra.underline,
    hyper: base.hyper || extra.hyper,
  };
}

export function wPara(inner: string, extraPr = ""): string {
  const pr = extraPr ? `<w:pPr>${extraPr}</w:pPr>` : "";
  return `<w:p>${pr}${inner || "<w:r/>"}</w:p>`;
}

export function headingPr(tag: string): string {
  const sz = HEAD_SZ[tag];
  return `<w:rPr><w:b/><w:sz w:val="${sz}"/><w:szCs w:val="${sz}"/></w:rPr>`;
}

export function legacyWordNote(path: string, data: Uint8Array): string | null {
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
