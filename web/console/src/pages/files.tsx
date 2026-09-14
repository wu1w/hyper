import { useEffect, useState } from "react";
import { api, failMsg } from "../api";
import { basename, fileHref } from "../media";
import { PreviewDock } from "../preview/PreviewDock";
import { Empty, Icon, Overlay, PageHead, uiConfirm } from "../ui";
import {
  parseWsRecents,
  pushRecentPath,
  recentLabel,
  treePadLeft,
  filesLocked,
  openEntryAction,
  WS_RECENTS_KEY,
  WS_RECENTS_MAX,
} from "../files-model";

type WsShortcut = { id: string; label: string; path: string };
type WsBrowse = { path: string; parent?: string | null; dirs: Array<{ name: string; path: string }> };

function readWsRecents(): string[] {
  try {
    return parseWsRecents(localStorage.getItem(WS_RECENTS_KEY));
  } catch {
    return [];
  }
}

function pushWsRecent(path: string): string[] {
  const next = pushRecentPath(path, readWsRecents(), WS_RECENTS_MAX);
  localStorage.setItem(WS_RECENTS_KEY, JSON.stringify(next));
  return next;
}

export function FilesPage({
  active = true,
  busy = false,
  workspace = "",
}: {
  active?: boolean;
  busy?: boolean;
  workspace?: string;
}) {
  const [entries, setEntries] = useState<Array<{ path: string; dir: boolean }>>([]);
  const [root, setRoot] = useState("");
  const [parent, setParent] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [sel, setSel] = useState("");
  const [previewMax, setPreviewMax] = useState(false);
  const [err, setErr] = useState("");
  const [shortcuts, setShortcuts] = useState<WsShortcut[]>([]);
  const [recents, setRecents] = useState<string[]>(readWsRecents);
  const [applying, setApplying] = useState(false);
  const [picking, setPicking] = useState(false);
  const [browse, setBrowse] = useState<WsBrowse | null>(null);
  const [browseErr, setBrowseErr] = useState("");
  const locked = filesLocked(applying, picking);

  const load = async () => {
    const [tree, ws] = await Promise.all([
      api<{ root: string; parent?: string | null; entries: Array<{ path: string; dir: boolean }> }>("/tree"),
      api<{ shortcuts?: WsShortcut[] }>("/workspace").catch(() => ({ shortcuts: [] as WsShortcut[] })),
    ]);
    setRoot(tree.root);
    setDraft(tree.root);
    setParent(tree.parent ?? null);
    setEntries(tree.entries || []);
    setShortcuts(ws.shortcuts || []);
  };

  useEffect(() => {
    if (active) void load().catch((e) => setErr(failMsg(e)));
  }, [active, workspace]);

  const applyPath = async (path: string) => {
    const p = path.trim();
    if (!p) return;
    setErr("");
    setApplying(true);
    try {
      const j = await api<{ ok?: boolean; cancelled?: boolean; workspace?: string }>("/workspace", {
        method: "POST",
        body: JSON.stringify({ path: p }),
      });
      if (j.cancelled) return;
      const next = j.workspace || p;
      setRecents(pushWsRecent(next));
      setSel("");
      await load();
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setApplying(false);
    }
  };

  const pickNative = async () => {
    setErr("");
    setPicking(true);
    try {
      const desktopPick = window.grokHyperDesktop?.pickFolder;
      if (desktopPick) {
        try {
          const r = await desktopPick();
          if (r?.cancelled || !r?.path) return;
          await applyPath(r.path);
          return;
        } catch {
          /* sidecar picker below */
        }
      }
      const j = await api<{ ok?: boolean; cancelled?: boolean; workspace?: string }>("/workspace/pick", {
        method: "POST",
      });
      if (j.cancelled) return;
      if (j.workspace) setRecents(pushWsRecent(j.workspace));
      setSel("");
      await load();
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setPicking(false);
    }
  };

  const openBrowse = async (path?: string) => {
    setBrowseErr("");
    try {
      const q = path ? `?path=${encodeURIComponent(path)}` : "";
      const j = await api<WsBrowse>(`/workspace/ls${q}`);
      setBrowse(j);
    } catch (e) {
      setBrowseErr(failMsg(e));
    }
  };

  const openFile = async (e: { path: string; dir: boolean }) => {
    const action = openEntryAction(e.dir, locked);
    if (action === "noop") return;
    if (action === "workspace") {
      const ok = await uiConfirm("把这个文件夹设为工作区？", e.path, { okLabel: "设为工作区" });
      if (ok) void applyPath(e.path);
      return;
    }
    setSel(e.path);
    setErr("");
  };

  useEffect(() => {
    if (!sel) setPreviewMax(false);
  }, [sel]);

  return (
    <div className={`page${previewMax ? " pv-maxed" : ""}`}>
      <PageHead
        title="文件"
        hint="工作区文件夹。换目录后 Read / Write / StrReplace 都跟过来，并写入当前会话 JSONL。"
        actions={
          <>
            {sel ? (
              <a className="btn ghost small" href={fileHref(sel, true)} download={basename(sel)}>
                <Icon name="download" />
                下载
              </a>
            ) : null}
            <button className="btn ghost small" onClick={() => void load().catch((e) => setErr(failMsg(e)))}>
              刷新
            </button>
          </>
        }
      />
      <div className="page-body" style={{ display: "flex", flexDirection: "column" }}>
        {busy ? (
          <div className="sub" style={{ margin: "0 0 8px" }}>
            当前会话正在跑一轮，换目录会失败。先等它结束或点停止，再打开；选定后会写入本会话，重启也不会回到旧目录。
          </div>
        ) : null}
        <div className="toolbar">
          <input
            className="input inline mono"
            aria-label="工作区路径"
            placeholder="绝对路径，或 ~/Documents"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !locked) void applyPath(draft);
            }}
            spellCheck={false}
            disabled={applying}
          />
          <button className="btn primary small" disabled={locked || !draft.trim()} onClick={() => void applyPath(draft)}>
            {applying ? "打开中…" : "打开"}
          </button>
          <button className="btn small" disabled={locked} onClick={() => void pickNative()} aria-label="系统选择文件夹">
            {picking ? "选择中…" : "系统选择"}
          </button>
          <button
            className="btn small"
            disabled={locked}
            onClick={() => void openBrowse(root || undefined)}
          >
            浏览
          </button>
          <button
            className="btn ghost small"
            disabled={locked || !parent}
            onClick={() => parent && void applyPath(parent)}
            aria-label="上级文件夹"
          >
            上级
          </button>
        </div>
        {busy ? <div className="sub">正在跑一轮，结束后才能换工作区。</div> : null}
        {shortcuts.length > 0 ? (
          <div className="ws-chips">
            {shortcuts.map((s) => (
              <button
                key={s.id}
                type="button"
                className="chip"
                disabled={locked}
                onClick={() => void applyPath(s.path)}
                title={s.path}
              >
                {s.label}
              </button>
            ))}
          </div>
        ) : null}
        {recents.length > 0 ? (
          <div className="ws-chips">
            {recents.map((p) => (
              <button
                key={p}
                type="button"
                className="chip mono"
                disabled={locked || p === root}
                onClick={() => void applyPath(p)}
                title={p}
              >
                {recentLabel(p)}
              </button>
            ))}
          </div>
        ) : null}
        {err ? <div className="err">{err}</div> : null}
        <div className="split tree-preview" style={{ flex: 1 }}>
          <div className="card" style={{ padding: 8, maxHeight: "70vh", overflow: "auto" }}>
            {parent ? (
              <button
                type="button"
                className="tree-row"
                disabled={locked}
                onClick={() => void applyPath(parent)}
              >
                <Icon name="folder" />
                ..
              </button>
            ) : null}
            {entries.map((e) => (
              <button
                key={e.path}
                type="button"
                className={`tree-row${sel === e.path ? " on" : ""}`}
                style={{ paddingLeft: treePadLeft(e.path) }}
                onClick={() => void openFile(e)}
              >
                <Icon name={e.dir ? "folder" : "file"} />
                {basename(e.path)}
              </button>
            ))}
          </div>
          <div className="card">
            {sel ? (
              <PreviewDock
                path={sel}
                layout="page"
                maximized={previewMax}
                onMaximize={setPreviewMax}
              />
            ) : (
              <Empty
                title="选择一个文件"
                body="左侧是当前文件夹。点开即可预览；Word / 表格 / PPT / PDF / 画布可在右侧改完后保存，覆盖工作区里的版本。"
              />
            )}
          </div>
        </div>
      </div>
      {browse ? (
        <Overlay onClose={() => setBrowse(null)}>
          <div className="modal wide" role="dialog" aria-labelledby="ws-browse-title">
            <h2 id="ws-browse-title">
              <Icon name="folder" />
              选择文件夹
            </h2>
            <div className="m-sub">{browse.path}</div>
            <div className="toolbar">
              <button
                className="btn ghost small"
                disabled={!browse.parent}
                onClick={() => browse.parent && void openBrowse(browse.parent)}
              >
                上级
              </button>
              <span className="spacer" />
              <button className="btn ghost small" onClick={() => setBrowse(null)}>
                取消
              </button>
              <button
                className="btn primary small"
                disabled={locked}
                onClick={() => {
                  const p = browse.path;
                  setBrowse(null);
                  void applyPath(p);
                }}
              >
                使用此文件夹
              </button>
            </div>
            {browseErr ? <div className="err">{browseErr}</div> : null}
            <div className="card" style={{ padding: 8, maxHeight: "46vh", overflow: "auto", margin: 0 }}>
              {browse.dirs.length === 0 ? (
                <Empty title="没有子文件夹" body="可以直接点「使用此文件夹」。" />
              ) : (
                browse.dirs.map((d) => (
                  <button
                    key={d.path}
                    type="button"
                    className="tree-row"
                    onClick={() => void openBrowse(d.path)}
                  >
                    <Icon name="folder" />
                    {d.name}
                  </button>
                ))
              )}
            </div>
          </div>
        </Overlay>
      ) : null}
    </div>
  );
}
