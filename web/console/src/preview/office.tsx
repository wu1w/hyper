import { useEffect, useId, useRef, useState } from "react";
import { failMsg } from "../api";
import type { PreviewProps } from "./kinds";

type DocsEditor = { destroyEditor: () => void };

declare global {
  interface Window {
    DocsAPI?: { DocEditor: new (id: string, cfg: Record<string, unknown>) => DocsEditor };
  }
}

type OfficeCfg = {
  docs_url: string;
  key: string;
  config: Record<string, unknown>;
};

const scripts = new Map<string, Promise<void>>();

function loadApi(docsUrl: string): Promise<void> {
  const src = `${docsUrl.replace(/\/+$/, "")}/web-apps/apps/api/documents/api.js`;
  const hit = scripts.get(src);
  if (hit) return hit;
  const p = new Promise<void>((resolve, reject) => {
    const existing = document.querySelector(`script[src="${src}"]`);
    if (existing && window.DocsAPI) {
      resolve();
      return;
    }
    const s = document.createElement("script");
    s.src = src;
    s.async = true;
    s.onload = () => resolve();
    s.onerror = () => reject(new Error("无法加载文档服务 api.js。"));
    document.head.appendChild(s);
  });
  scripts.set(src, p);
  p.catch(() => {
    scripts.delete(src);
  });
  return p;
}

export function OfficePreview({ path, onDirty, registerExport, onOfficeKey }: PreviewProps) {
  const rawId = useId().replace(/:/g, "");
  const hostId = `oo-${rawId}`;
  const ed = useRef<DocsEditor | null>(null);
  const keyRef = useRef("");
  const [err, setErr] = useState("");
  const [msg, setMsg] = useState("正在连接文档服务…");

  useEffect(() => {
    registerExport(async () => {
      throw new Error("office");
    });
  }, [registerExport]);

  useEffect(() => {
    let gone = false;
    setErr("");
    setMsg("正在连接文档服务…");
    onDirty(false);
    (async () => {
      try {
        const r = await fetch(`/api/office/config?path=${encodeURIComponent(path)}`);
        if (!r.ok) throw new Error(await r.text());
        const j = (await r.json()) as OfficeCfg;
        if (gone) return;
        keyRef.current = j.key;
        onOfficeKey?.(j.key);
        await loadApi(j.docs_url);
        if (gone) return;
        if (!window.DocsAPI) throw new Error("DocsAPI 未就绪");
        ed.current?.destroyEditor();
        ed.current = new window.DocsAPI.DocEditor(hostId, {
          ...j.config,
          events: {
            onDocumentStateChange: (e: { data?: boolean }) => {
              if (e.data) onDirty(true);
            },
            onError: (e: { data?: { errorDescription?: string } }) => {
              setErr(e.data?.errorDescription || "文档服务报错");
            },
          },
        });
        setMsg("");
      } catch (e) {
        if (!gone) setErr(failMsg(e));
      }
    })();
    return () => {
      gone = true;
      try {
        ed.current?.destroyEditor();
      } catch {
        /* editor already gone */
      }
      ed.current = null;
    };
  }, [path, hostId, onDirty, onOfficeKey]);

  return (
    <div className="pv-office">
      {err ? <div className="err">{err}</div> : null}
      {msg ? <div className="sub">{msg}</div> : null}
      <div id={hostId} className="pv-office-host" />
    </div>
  );
}
