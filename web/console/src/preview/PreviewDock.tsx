import { useEffect, useRef, useState, type ComponentType } from "react";
import { api, failMsg } from "../api";
import { fileHref, siteHref, basename } from "../media";
import { Icon } from "../ui";
import { isOfficeKind, kindFor, type PreviewKind, type PreviewProps } from "./kinds";
import { loadPreviewBytes, savePreview } from "./save";

type OfficeStatus = { ready?: boolean; starting?: boolean; docs_url?: string; hint?: string | null };

export function PreviewDock({
  path,
  rev = "",
  onClose,
  layout = "page",
  maximized = false,
  onMaximize,
}: {
  path: string;
  rev?: string;
  onClose?: () => void;
  layout?: "chat" | "page";
  maximized?: boolean;
  onMaximize?: (v: boolean) => void;
}) {
  const kind = kindFor(path);
  const [bytes, setBytes] = useState<Uint8Array | null>(null);
  const [View, setView] = useState<ComponentType<PreviewProps> | null>(null);
  const [office, setOffice] = useState(false);
  const [officeKey, setOfficeKey] = useState("");
  const [dirty, setDirty] = useState(false);
  const [err, setErr] = useState("");
  const [saving, setSaving] = useState(false);
  const [note, setNote] = useState("");
  const [boot, setBoot] = useState(0);
  const exporter = useRef<() => Promise<Uint8Array>>(async () => new Uint8Array());
  const hostRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let gone = false;
    setErr("");
    setDirty(false);
    setNote("");
    setBytes(null);
    setView(null);
    setOffice(false);
    setOfficeKey("");
    (async () => {
      try {
        let useOffice = false;
        if (isOfficeKind(kind.id)) {
          const st = await api<OfficeStatus>("/office/status").catch(() => ({ ready: false } as OfficeStatus));
          useOffice = !!st.ready;
          if (!useOffice && st.hint) setNote(String(st.hint));
        }
        if (gone) return;
        if (useOffice) {
          try {
            const { OfficePreview } = await import("./office");
            if (gone) return;
            setOffice(true);
            setBytes(new Uint8Array([1]));
            setView(() => OfficePreview);
            setNote("");
            return;
          } catch (e) {
            useOffice = false;
            setErr(failMsg(e));
            setNote("OnlyOffice 模块加载失败，已改用内置预览。若刚重新编译过控制台，请强制刷新。");
          }
        }
        const [{ bytes: buf, truncated }, Comp] = await Promise.all([loadPreviewBytes(path), kind.load()]);
        if (gone) return;
        setBytes(buf);
        setView(() => Comp);
        if (truncated) setNote("文件超过 96MB，预览是截断后的内容，可能打不开。");
      } catch (e) {
        if (!gone) setErr(failMsg(e));
      }
    })();
    return () => {
      gone = true;
    };
  }, [path, kind, boot, rev]);

  useEffect(() => {
    if (office || !isOfficeKind(kind.id)) return;
    let stop = false;
    let timer = 0;
    const tick = async () => {
      const st = await api<OfficeStatus>("/office/status").catch(() => null);
      if (stop || !st) return;
      if (st.ready) {
        if (dirty) {
          setNote("文档服务已就绪。保存或关闭后重新打开即可完整编辑。");
        } else {
          setBoot((n) => n + 1);
        }
        return;
      }
      if (st.hint) setNote(String(st.hint));
      if (st.starting) timer = window.setTimeout(() => void tick(), 4000);
    };
    timer = window.setTimeout(() => void tick(), 4000);
    return () => {
      stop = true;
      window.clearTimeout(timer);
    };
  }, [path, kind, office, dirty, boot]);

  useEffect(() => {
    if (!maximized) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onMaximize?.(false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [maximized, onMaximize]);

  useEffect(() => {
    document.body.classList.toggle("hyper-pv-max", maximized);
    return () => document.body.classList.remove("hyper-pv-max");
  }, [maximized]);

  useEffect(() => {
    if (!maximized) return;
    const pane = hostRef.current?.closest(".main-pane");
    if (!pane) return;
    const obs = new MutationObserver(() => {
      if (pane.hasAttribute("hidden")) onMaximize?.(false);
    });
    obs.observe(pane, { attributes: true, attributeFilter: ["hidden"] });
    return () => obs.disconnect();
  }, [maximized, onMaximize]);

  const save = async () => {
    setSaving(true);
    setErr("");
    try {
      if (office) {
        await api("/office/forcesave", {
          method: "POST",
          body: JSON.stringify({ path, key: officeKey }),
        });
        setDirty(false);
        setNote("已覆盖保存，下一轮对话会按这个版本 Read。");
        return;
      }
      const data = await exporter.current();
      await savePreview(path, data, kind.id);
      setDirty(false);
      setNote("已覆盖保存，下一轮对话会按这个版本 Read。");
      setBoot((n) => n + 1);
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setSaving(false);
    }
  };

  const canSave = kind.editable && (office ? !!officeKey : !!bytes);

  return (
    <div
      ref={hostRef}
      className={`pv-dock${maximized ? " max" : ""}${office ? " oo" : ""}${layout === "chat" ? " chat" : ""}`}
    >
      <div className="pv-bar">
        <span className="pv-kind">{kind.label}</span>
        <span className="pv-name" title={path}>
          {basename(path)}
        </span>
        {office ? <span className="pv-oo">OnlyOffice</span> : null}
        {dirty ? <span className="pv-dirty">未保存</span> : null}
        <span className="spacer" />
        <a className="btn ghost small" href={fileHref(path, true)} download={basename(path)}>
          下载
        </a>
        {kind.id === "browser" ? (
          <a className="btn ghost small" href={siteHref(path, rev)} target="_blank" rel="noreferrer">
            新窗口
          </a>
        ) : null}
        {kind.editable ? (
          <button type="button" className="btn small" disabled={saving || !canSave} onClick={() => void save()}>
            {saving ? "保存中" : "保存"}
          </button>
        ) : null}
        <button
          type="button"
          className="icon-btn"
          aria-label={maximized ? "还原预览" : "最大化预览"}
          title={maximized ? "还原" : "最大化"}
          onClick={() => onMaximize?.(!maximized)}
        >
          <Icon name={maximized ? "restore" : "maximize"} />
        </button>
        {onClose ? (
          <button type="button" className="icon-btn" aria-label="关闭预览" onClick={onClose}>
            <Icon name="x" />
          </button>
        ) : null}
      </div>
      {kind.hint && !maximized ? <div className="pv-hint">{kind.hint}</div> : null}
      {err ? <div className="err">{err}</div> : null}
      {note ? <div className="sub">{note}</div> : null}
      <div className="pv-body">
        {bytes && View ? (
          <View
            key={`${path}:${rev}:${boot}`}
            path={path}
            bytes={bytes}
            url={kind.id === "browser" ? siteHref(path, `${rev}:${boot}`) : fileHref(path)}
            onDirty={setDirty}
            onOfficeKey={setOfficeKey}
            registerExport={(fn) => {
              exporter.current = fn;
            }}
          />
        ) : (
          <div className="sub">加载预览…</div>
        )}
      </div>
    </div>
  );
}
