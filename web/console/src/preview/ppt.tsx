import { useEffect, useRef, useState } from "react";
import type { PreviewProps } from "./kinds";
// DEPS: jszip @aiden0z/pptx-renderer
import JSZip from "jszip";
import { PptxViewer, RECOMMENDED_ZIP_LIMITS } from "@aiden0z/pptx-renderer";

const SLIDE_NAME = /^ppt\/slides\/slide(\d+)\.xml$/i;

type SlideDoc = {
  name: string;
  xml: string;
  texts: string[];
};

function atRe(): RegExp {
  return /<a:t([^>]*?)\/>|<a:t([^>]*)>([\s\S]*?)<\/a:t>/g;
}

function escapeXml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

function unescapeXml(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&#x([0-9a-fA-F]+);/g, (_, h: string) => String.fromCodePoint(parseInt(h, 16)))
    .replace(/&#(\d+);/g, (_, d: string) => String.fromCodePoint(Number(d)))
    .replace(/&amp;/g, "&");
}

function extractAt(xml: string): string[] {
  const texts: string[] = [];
  const re = atRe();
  let m: RegExpExecArray | null;
  while ((m = re.exec(xml))) {
    texts.push(unescapeXml(m[3] ?? ""));
  }
  return texts;
}

function replaceAt(xml: string, texts: string[]): string {
  let i = 0;
  return xml.replace(atRe(), (full, selfAttrs: string | undefined, openAttrs: string | undefined) => {
    if (i >= texts.length) return full;
    const attrs = selfAttrs ?? openAttrs ?? "";
    return `<a:t${attrs}>${escapeXml(texts[i++])}</a:t>`;
  });
}

function slideNum(name: string): number {
  const m = name.match(SLIDE_NAME);
  return m ? Number(m[1]) : 0;
}

function looksLikeZip(bytes: Uint8Array): boolean {
  return bytes.length > 4 && bytes[0] === 0x50 && bytes[1] === 0x4b;
}

async function parsePptx(bytes: Uint8Array): Promise<SlideDoc[]> {
  const zip = await JSZip.loadAsync(bytes);
  const names = Object.keys(zip.files)
    .filter((n) => SLIDE_NAME.test(n) && !zip.files[n]?.dir)
    .sort((a, b) => slideNum(a) - slideNum(b));
  const slides: SlideDoc[] = [];
  for (const name of names) {
    const f = zip.file(name);
    if (!f) continue;
    const xml = await f.async("string");
    slides.push({ name, xml, texts: extractAt(xml) });
  }
  return slides;
}

async function exportPptx(bytes: Uint8Array, slides: SlideDoc[]): Promise<Uint8Array> {
  const zip = await JSZip.loadAsync(bytes);
  for (const s of slides) {
    zip.file(s.name, replaceAt(s.xml, s.texts));
  }
  return zip.generateAsync({
    type: "uint8array",
    compression: "DEFLATE",
    compressionOptions: { level: 6 },
  });
}

function errText(e: unknown): string {
  const msg = e instanceof Error ? e.message : String(e);
  return `无法打开幻灯片：${msg}`;
}

export function PptPreview({ path, bytes, onDirty, registerExport }: PreviewProps) {
  const stageRef = useRef<HTMLDivElement>(null);
  const viewerRef = useRef<PptxViewer | null>(null);
  const slidesRef = useRef<SlideDoc[]>([]);
  const [slides, setSlides] = useState<SlideDoc[]>([]);
  const [idx, setIdx] = useState(0);
  const [count, setCount] = useState(0);
  const [err, setErr] = useState("");
  const [ready, setReady] = useState(false);
  slidesRef.current = slides;

  useEffect(() => {
    registerExport(async () => exportPptx(bytes, slidesRef.current));
  }, [bytes, registerExport]);

  useEffect(() => {
    let gone = false;
    setReady(false);
    setErr("");
    setSlides([]);
    setIdx(0);
    setCount(0);
    void parsePptx(bytes)
      .then((next) => {
        if (!gone) setSlides(next);
      })
      .catch(() => {
        /* graphical open reports the real error */
      });
    return () => {
      gone = true;
    };
  }, [bytes]);

  useEffect(() => {
    const el = stageRef.current;
    if (!el) return;
    let gone = false;
    let viewer: PptxViewer | undefined;
    setReady(false);
    setErr("");
    setCount(0);
    el.replaceChildren();

    const n = path.toLowerCase();
    if (n.endsWith(".odp")) {
      setErr("OpenDocument 演示文稿（.odp）无法在浏览器里画幻灯片，请另存为 .pptx。");
      setReady(true);
      return;
    }
    if (!looksLikeZip(bytes)) {
      setErr("这不是 PPTX 压缩包。旧版 .ppt 无法在浏览器里画幻灯片，请另存为 .pptx。");
      setReady(true);
      return;
    }

    void (async () => {
      try {
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
        if (gone) return;
        const opened = await PptxViewer.open(bytes.slice(), el, {
          renderMode: "slide",
          fitMode: "contain",
          zipLimits: RECOMMENDED_ZIP_LIMITS,
          lazySlides: false,
          lazyMedia: true,
          pdfjs: false,
          onSlideError: (_index, error) => {
            if (!gone) setErr(errText(error));
          },
        });
        if (gone) {
          opened.destroy();
          return;
        }
        viewer = opened;
        viewerRef.current = opened;
        setCount(opened.slideCount);
        if (opened.slideCount < 1) {
          setErr("未找到幻灯片。");
        }
        setReady(true);
      } catch (e) {
        if (!gone) {
          setErr(errText(e));
          setReady(true);
        }
      }
    })();

    return () => {
      gone = true;
      viewerRef.current = null;
      viewer?.destroy();
    };
  }, [bytes, path]);

  useEffect(() => {
    const viewer = viewerRef.current;
    if (!viewer || !ready || count < 1) return;
    const next = Math.min(idx, count - 1);
    if (viewer.currentSlideIndex === next) return;
    void viewer.goToSlide(next);
  }, [idx, ready, count]);

  const total = Math.max(count, slides.length);
  const cur = slides.length > 0 ? slides[Math.min(idx, slides.length - 1)] : undefined;
  const page = total > 0 ? Math.min(idx, total - 1) + 1 : 0;

  const setFrame = (ti: number, value: string) => {
    setSlides((prev) =>
      prev.map((s, si) => (si !== idx ? s : { ...s, texts: s.texts.map((t, j) => (j === ti ? value : t)) })),
    );
    onDirty(true);
  };

  return (
    <div className="pv-ppt">
      <div className="pv-bar">
        <button
          type="button"
          className="btn ghost small"
          disabled={!ready || idx <= 0}
          aria-label="上一张幻灯片"
          onClick={() => setIdx((i) => Math.max(0, i - 1))}
        >
          上一张
        </button>
        <span className="sub">
          {ready && !err && total > 0 ? `幻灯片 ${page} / ${total}` : path}
        </span>
        <button
          type="button"
          className="btn ghost small"
          disabled={!ready || total < 1 || idx >= total - 1}
          aria-label="下一张幻灯片"
          onClick={() => setIdx((i) => Math.min(total - 1, i + 1))}
        >
          下一张
        </button>
      </div>
      {!ready ? <p className="sub">正在绘制幻灯片…</p> : null}
      {err ? <p className="sub">{err}</p> : null}
      <div className="pv-ppt-stage" ref={stageRef} aria-label={`${path} 幻灯片预览`} />
      {cur && cur.texts.length > 0 ? (
        <details className="pv-ppt-edit">
          <summary>改文案</summary>
          <p className="sub">改的是文本框内容；保存后才写回 pptx。画面不会跟着每个字刷新。</p>
          {cur.texts.map((t, i) => (
            <textarea
              key={`${cur.name}:${i}`}
              className="pv-text"
              value={t}
              spellCheck={false}
              aria-label={`幻灯片 ${page} 文本 ${i + 1}`}
              onChange={(e) => setFrame(i, e.target.value)}
            />
          ))}
        </details>
      ) : null}
    </div>
  );
}
