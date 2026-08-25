import { useEffect, useState } from "react";
import JSZip from "jszip";
import type { PreviewProps } from "./kinds";
import { looksLikeOle, looksLikeZip } from "./bytes";

function xmlText(xml: string): string {
  return xml
    .replace(/<[^>]+>/g, " ")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&amp;/g, "&")
    .replace(/\s+/g, " ")
    .trim();
}

async function vsdxOutline(bytes: Uint8Array, path: string): Promise<string> {
  if (looksLikeOle(bytes) || path.toLowerCase().endsWith(".vsd")) {
    return "旧版 .vsd 是 Visio 二进制，浏览器无法打开。请另存为 .vsdx。";
  }
  if (!looksLikeZip(bytes)) {
    return `无法识别 ${path}（${bytes.length.toLocaleString()} 字节）。`;
  }
  const zip = await JSZip.loadAsync(bytes);
  const names = Object.keys(zip.files).filter((n) => !zip.files[n]?.dir);
  const pageFiles = names
    .filter((n) => /^visio\/pages\/page\d+\.xml$/i.test(n))
    .sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
  const lines: string[] = [`vsdx ${path}（${bytes.length.toLocaleString()} 字节，${names.length} 个部件）`];
  if (!pageFiles.length) {
    lines.push("没有 visio/pages/pageN.xml。可能不是标准 vsdx。");
    return lines.join("\n");
  }
  for (const name of pageFiles) {
    const xml = await zip.file(name)?.async("string");
    const label = name.replace(/^visio\/pages\//i, "");
    const preview = xml ? xmlText(xml).slice(0, 180) : "";
    lines.push(preview ? `${label}: ${preview}` : label);
  }
  return lines.join("\n");
}

export function VisioPreview({ bytes, path, registerExport }: PreviewProps) {
  const [outline, setOutline] = useState("正在读取大纲…");
  useEffect(() => {
    registerExport(async () => bytes);
  }, [bytes, registerExport]);
  useEffect(() => {
    let gone = false;
    void vsdxOutline(bytes, path)
      .then((text) => {
        if (!gone) setOutline(text);
      })
      .catch((e) => {
        if (!gone) setOutline(e instanceof Error ? e.message : String(e));
      });
    return () => {
      gone = true;
    };
  }, [bytes, path]);
  return (
    <div className="pv-fallback">
      <p className="sub">Visio 只读大纲（不内嵌 Visio 内核）</p>
      <pre className="pre">{outline}</pre>
    </div>
  );
}
