// DEPS: pdfjs-dist pdf-lib
import { useEffect, useRef, useState } from "react";
import { PDFDocument, PDFHexString } from "pdf-lib";
import { getDocument, GlobalWorkerOptions } from "pdfjs-dist";
import pdfWorker from "pdfjs-dist/build/pdf.worker.min.mjs?url";
import type { PreviewProps } from "./kinds";
import { copyU8, looksLikePdf } from "./bytes";

GlobalWorkerOptions.workerSrc = pdfWorker;

type PdfJsDoc = Awaited<ReturnType<typeof getDocument>["promise"]>;

async function stampUserNote(bytes: Uint8Array, notes: string): Promise<Uint8Array> {
  const trimmed = notes.trim();
  if (!trimmed) return bytes;
  const pdf = await PDFDocument.load(bytes);
  const pages = pdf.getPages();
  if (pages.length === 0) return bytes;
  const page = pages[0];
  const label = `用户注：${trimmed}`;
  const { width, height } = page.getSize();
  try {
    page.drawText(label, { x: 36, y: 20, size: 9, maxWidth: Math.max(64, width - 72) });
  } catch {
    /* Helvetica cannot encode CJK; the text annotation still stores the note. */
  }
  const annot = pdf.context.obj({
    Type: "Annot",
    Subtype: "Text",
    Rect: [24, height - 44, 48, height - 20],
    Contents: PDFHexString.fromText(label),
    Name: "Comment",
    Open: false,
    C: [1, 0.85, 0.2],
    F: 4,
  });
  page.node.addAnnot(pdf.context.register(annot));
  const saved = await pdf.save();
  return saved instanceof Uint8Array ? saved : new Uint8Array(saved);
}

export function PdfPreview({ path, bytes, url, onDirty, registerExport }: PreviewProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const pdfRef = useRef<PdfJsDoc | null>(null);
  const notesRef = useRef("");
  const [page, setPage] = useState(1);
  const [pageCount, setPageCount] = useState(0);
  const [msg, setMsg] = useState("正在打开 PDF…");
  const [notes, setNotes] = useState("");

  useEffect(() => {
    registerExport(async () => stampUserNote(bytes, notesRef.current));
  }, [bytes, registerExport]);

  useEffect(() => {
    let cancelled = false;
    pdfRef.current = null;
    setPage(1);
    setPageCount(0);
    setMsg("正在打开 PDF…");
    if (!looksLikePdf(bytes)) {
      setMsg("不是 PDF 文件（缺少 %PDF 头）。");
      return () => {
        cancelled = true;
      };
    }
    const task = getDocument({ data: copyU8(bytes) });
    (async () => {
      try {
        const pdf = await task.promise;
        if (cancelled) {
          void pdf.destroy();
          return;
        }
        pdfRef.current = pdf;
        setPageCount(pdf.numPages);
        setMsg("");
      } catch (e) {
        if (!cancelled) setMsg(e instanceof Error ? e.message : "无法打开 PDF");
      }
    })();
    return () => {
      cancelled = true;
      pdfRef.current = null;
      void task.destroy();
    };
  }, [bytes]);

  useEffect(() => {
    const pdf = pdfRef.current;
    const canvas = canvasRef.current;
    if (!pdf || !canvas || pageCount < 1) return;
    let cancelled = false;
    let renderTask: { cancel: () => void } | undefined;
    (async () => {
      try {
        const pdfPage = await pdf.getPage(page);
        if (cancelled) return;
        const viewport = pdfPage.getViewport({ scale: 1.25 });
        canvas.width = viewport.width;
        canvas.height = viewport.height;
        const ctx = canvas.getContext("2d");
        if (!ctx) {
          setMsg("无法渲染页面");
          return;
        }
        renderTask = pdfPage.render({
          canvasContext: ctx,
          viewport,
          canvas,
        } as Parameters<typeof pdfPage.render>[0]);
        await renderTask.promise;
        if (!cancelled) setMsg("");
      } catch (e) {
        const name = e && typeof e === "object" && "name" in e ? String((e as { name: string }).name) : "";
        if (cancelled || name === "RenderingCancelledException") return;
        setMsg(e instanceof Error ? e.message : "无法渲染页面");
      }
    })();
    return () => {
      cancelled = true;
      renderTask?.cancel();
    };
  }, [page, pageCount, bytes]);

  return (
    <div className="pv-split">
      <div className="pv-bar">
        <button
          type="button"
          className="btn ghost small"
          disabled={page <= 1}
          onClick={() => setPage((n) => Math.max(1, n - 1))}
        >
          上一页
        </button>
        <span className="sub">
          第 {page} / {pageCount || "—"} 页
        </span>
        <button
          type="button"
          className="btn ghost small"
          disabled={pageCount < 1 || page >= pageCount}
          onClick={() => setPage((n) => Math.min(pageCount, n + 1))}
        >
          下一页
        </button>
        <span className="spacer" />
        <a className="btn ghost small" href={url} target="_blank" rel="noreferrer">
          新窗口
        </a>
      </div>
      {msg ? <p className="sub">{msg}</p> : null}
      <canvas ref={canvasRef} className="pv-img" aria-label={`${path} 第 ${page} 页`} />
      <label className="sub" htmlFor="pv-pdf-notes">
        用户注
      </label>
      <textarea
        id="pv-pdf-notes"
        className="pv-text"
        value={notes}
        spellCheck={false}
        placeholder="保存时写入第 1 页（用户注：…）"
        onChange={(e) => {
          const v = e.target.value;
          notesRef.current = v;
          setNotes(v);
          onDirty(v.trim().length > 0);
        }}
      />
    </div>
  );
}
