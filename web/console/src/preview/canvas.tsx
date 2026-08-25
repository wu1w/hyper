import { useEffect, useRef, useState } from "react";
import type { PreviewProps } from "./kinds";

export type CanvasDoc = {
  v: 1;
  nodes: Array<{ id: string; x: number; y: number; w: number; h: number; text: string }>;
};

function parseDoc(bytes: Uint8Array): CanvasDoc {
  try {
    const j = JSON.parse(new TextDecoder().decode(bytes)) as CanvasDoc;
    if (j && j.v === 1 && Array.isArray(j.nodes)) return j;
  } catch {
    /* empty */
  }
  return { v: 1, nodes: [{ id: "n1", x: 40, y: 40, w: 180, h: 80, text: "便签" }] };
}

export function CanvasPreview({ bytes, onDirty, registerExport }: PreviewProps) {
  const [doc, setDoc] = useState(() => parseDoc(bytes));
  const docRef = useRef(doc);
  docRef.current = doc;
  useEffect(() => {
    setDoc(parseDoc(bytes));
  }, [bytes]);
  useEffect(() => {
    registerExport(async () => new TextEncoder().encode(JSON.stringify(docRef.current, null, 2)));
  }, [registerExport]);
  return (
    <div className="pv-canvas">
      {doc.nodes.map((n, i) => (
        <textarea
          key={n.id}
          className="pv-sticky"
          style={{ left: n.x, top: n.y, width: n.w, height: n.h }}
          value={n.text}
          onChange={(e) => {
            const next = { ...doc, nodes: doc.nodes.map((x, j) => (j === i ? { ...x, text: e.target.value } : x)) };
            setDoc(next);
            onDirty(true);
          }}
        />
      ))}
      <button
        type="button"
        className="btn ghost small pv-canvas-add"
        onClick={() => {
          setDoc({
            ...doc,
            nodes: [...doc.nodes, { id: `n${Date.now()}`, x: 48 + doc.nodes.length * 12, y: 48, w: 180, h: 80, text: "新便签" }],
          });
          onDirty(true);
        }}
      >
        添加便签
      </button>
    </div>
  );
}
