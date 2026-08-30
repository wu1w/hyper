import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as vscode from "vscode";
import { ChatViewProvider } from "./chatView";
import {
  isEditTool,
  parseToolArgs,
  registerOldFiles,
  revealEdit,
  snapshotFile,
  workspaceRoot,
  type OldFileProvider,
} from "./editor";
import { HyperSidecar, type EditorContext, type SessionEvent } from "./sidecar";

const SESSION_KEY = "hyper.sessionId";

type Snap = { rel: string; before: string };

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel("Hyper");
  const oldFiles = registerOldFiles(context);
  const snaps = new Map<string, Snap>();
  let sidecar: HyperSidecar | undefined;
  let unsub: (() => void) | undefined;

  const chat = new ChatViewProvider(context.extensionUri, (msg) => {
    const type = String(msg.type || "");
    if (type === "send") {
      void runTurn(String(msg.text || ""));
    } else if (type === "abort") {
      void sidecar?.turnAbort().catch((e) => output.appendLine(String(e)));
    } else if (type === "open") {
      void openRel(String(msg.path || ""));
    }
  });

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewId, chat, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    output,
    vscode.commands.registerCommand("hyper.newChat", () => {
      void boot(true);
    }),
    vscode.commands.registerCommand("hyper.abort", () => {
      void sidecar?.turnAbort().catch((e) => output.appendLine(String(e)));
    }),
    { dispose: () => void sidecar?.close() },
  );

  async function boot(fresh: boolean): Promise<void> {
    const workspace = workspaceRoot();
    if (!workspace) {
      chat.post({ type: "status", text: "先打开一个文件夹", busy: false });
      void vscode.window.showWarningMessage("Hyper 需要一个工作区文件夹。");
      return;
    }
    unsub?.();
    if (sidecar) {
      await sidecar.close().catch(() => undefined);
      sidecar = undefined;
    }
    if (fresh) {
      chat.post({ type: "reset" });
    }
    const session = fresh ? newSessionId(workspace) : loadSession(context, workspace);
    void context.workspaceState.update(SESSION_KEY, session);
    const command = resolveHyperCommand(context);
    sidecar = HyperSidecar.spawn({
      workspace,
      session,
      command,
      onStderr: (line) => output.appendLine(line),
    });
    unsub = sidecar.onEvent((ev) => onEvent(ev, oldFiles, snaps, chat));
    try {
      const opened = await sidecar.sessionOpen({ session, workspace, mode: "agent" });
      const id = opened.session || session;
      void context.workspaceState.update(SESSION_KEY, id);
      chat.post({ type: "ready", workspace, session: id });
      chat.post({ type: "status", text: "就绪", busy: false });
    } catch (e) {
      const text = e instanceof Error ? e.message : String(e);
      output.appendLine(text);
      chat.post({ type: "error", text: `sidecar 启动失败：${text}` });
      chat.post({ type: "status", text: "未连接", busy: false });
    }
  }

  async function runTurn(text: string): Promise<void> {
    const t = text.trim();
    if (!t) {
      return;
    }
    if (!sidecar) {
      await boot(false);
    }
    if (!sidecar) {
      return;
    }
    chat.post({ type: "user", text: t });
    try {
      if (t.startsWith("/")) {
        await sidecar.slash(t);
        chat.post({ type: "status", text: "就绪", busy: false });
        return;
      }
      const r = await sidecar.turnStart(t, collectEditor());
      if (r.queued) {
        chat.post({
          type: "status",
          text: `已排队 · ${r.n ?? 1}`,
          busy: true,
        });
      } else {
        chat.post({ type: "status", text: "运行中", busy: true });
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      chat.post({ type: "error", text: msg });
      chat.post({ type: "status", text: "就绪", busy: false });
    }
  }

  void boot(false);
}

function resolveHyperCommand(context: vscode.ExtensionContext): string {
  const cfg = vscode.workspace.getConfiguration("hyper").get<string>("command")?.trim();
  if (cfg && cfg !== "hyper") {
    return cfg;
  }
  const envBin = process.env.HYPER_BIN?.trim();
  if (envBin) {
    return envBin;
  }
  try {
    const marker = fs.readFileSync(path.join(context.extensionPath, "hyper.bin"), "utf8").trim();
    if (marker) {
      return marker;
    }
  } catch {
    /* installed without hyper vscode-install */
  }
  return cfg || "hyper";
}

function onEvent(
  ev: SessionEvent,
  oldFiles: OldFileProvider,
  snaps: Map<string, Snap>,
  chat: ChatViewProvider,
): void {
  chat.post({ type: "event", event: ev });
  if (ev.type === "assistant") {
    for (const c of ev.tool_calls || []) {
      const name = c.function?.name || "";
      if (!isEditTool(name)) {
        continue;
      }
      const args = parseToolArgs(c.function?.arguments);
      const rel = typeof args.path === "string" ? args.path : "";
      if (!rel || !c.id) {
        continue;
      }
      snaps.set(c.id, { rel, before: snapshotFile(rel) });
    }
  }
  if (ev.type === "tool" && ev.tool_call_id) {
    const snap = snaps.get(ev.tool_call_id);
    if (snap) {
      snaps.delete(ev.tool_call_id);
      void revealEdit(oldFiles, ev.tool_call_id, snap.rel, snap.before);
    }
  }
  if (ev.type === "stop") {
    chat.post({ type: "status", text: "就绪", busy: false });
  }
}

async function openRel(rel: string): Promise<void> {
  const root = workspaceRoot();
  if (!root || !rel) {
    return;
  }
  const uri = vscode.Uri.joinPath(vscode.Uri.file(root), rel.replace(/^\/+/, ""));
  await vscode.window.showTextDocument(uri, { preview: true, preserveFocus: true });
}

function collectEditor(): EditorContext {
  const root = workspaceRoot();
  if (!root) {
    return { files: [] };
  }
  const byPath = new Map<string, { line?: number; selection?: string }>();
  const relOf = (uri: vscode.Uri): string | undefined => {
    if (uri.scheme !== "file") {
      return undefined;
    }
    const rel = path.relative(root, uri.fsPath).replace(/\\/g, "/");
    if (!rel || rel.startsWith("..")) {
      return undefined;
    }
    return rel;
  };
  for (const ed of vscode.window.visibleTextEditors) {
    const rel = relOf(ed.document.uri);
    if (!rel) {
      continue;
    }
    const sel = ed.selection;
    const selection = sel.isEmpty ? undefined : ed.document.getText(sel).slice(0, 800);
    byPath.set(rel, {
      line: sel.active.line + 1,
      selection: selection?.trim() ? selection : undefined,
    });
  }
  for (const group of vscode.window.tabGroups.all) {
    for (const tab of group.tabs) {
      const input = tab.input;
      if (input instanceof vscode.TabInputText) {
        const rel = relOf(input.uri);
        if (rel && !byPath.has(rel)) {
          byPath.set(rel, {});
        }
      }
    }
  }
  const activeRel = vscode.window.activeTextEditor
    ? relOf(vscode.window.activeTextEditor.document.uri)
    : undefined;
  const files = [...byPath.entries()].map(([p, extra]) => ({
    path: p,
    line: extra.line,
    selection: extra.selection,
  }));
  if (activeRel) {
    const i = files.findIndex((f) => f.path === activeRel);
    if (i > 0) {
      const [f] = files.splice(i, 1);
      files.unshift(f);
    }
  }
  return { active: activeRel, files: files.slice(0, 12) };
}

function loadSession(context: vscode.ExtensionContext, workspace: string): string {
  const saved = context.workspaceState.get<string>(SESSION_KEY);
  if (saved) {
    return saved;
  }
  return newSessionId(workspace);
}

function newSessionId(workspace: string): string {
  const h = crypto.createHash("sha1").update(workspace).digest("hex").slice(0, 10);
  return `vscode-${h}-${Date.now().toString(36)}`;
}

export function deactivate(): void {}
