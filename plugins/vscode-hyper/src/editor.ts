import * as fs from "node:fs";
import * as path from "node:path";
import * as vscode from "vscode";

const SCHEME = "hyper-old";

export class OldFileProvider implements vscode.TextDocumentContentProvider {
  private readonly contents = new Map<string, string>();
  readonly onDidChangeEmitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.onDidChangeEmitter.event;

  put(callId: string, rel: string, body: string): vscode.Uri {
    const key = `${callId}:${rel}`;
    this.contents.set(key, body);
    const uri = vscode.Uri.from({
      scheme: SCHEME,
      path: `/${rel}`,
      query: key,
    });
    this.onDidChangeEmitter.fire(uri);
    return uri;
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.query) ?? "";
  }
}

export function registerOldFiles(context: vscode.ExtensionContext): OldFileProvider {
  const provider = new OldFileProvider();
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, provider),
  );
  return provider;
}

export function workspaceRoot(): string | undefined {
  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

export function resolveInWorkspace(rel: string): vscode.Uri | undefined {
  const root = workspaceRoot();
  if (!root) {
    return undefined;
  }
  const clean = rel.replace(/\\/g, "/").replace(/^\/+/, "");
  return vscode.Uri.file(path.join(root, clean));
}

export function snapshotFile(rel: string): string {
  const uri = resolveInWorkspace(rel);
  if (!uri) {
    return "";
  }
  try {
    return fs.readFileSync(uri.fsPath, "utf8");
  } catch {
    return "";
  }
}

export async function revealEdit(
  provider: OldFileProvider,
  callId: string,
  rel: string,
  before: string,
): Promise<void> {
  const fileUri = resolveInWorkspace(rel);
  if (!fileUri) {
    return;
  }
  const oldUri = provider.put(callId, rel, before);
  const opts = { preview: true, preserveFocus: true, viewColumn: vscode.ViewColumn.One };
  try {
    await vscode.commands.executeCommand("vscode.diff", oldUri, fileUri, `${rel} (Hyper)`, opts);
  } catch {
    try {
      await vscode.window.showTextDocument(fileUri, opts);
    } catch {
      /* file may have been deleted */
    }
  }
}

export function parseToolArgs(raw: unknown): Record<string, unknown> {
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    return raw as Record<string, unknown>;
  }
  if (typeof raw !== "string") {
    return {};
  }
  try {
    const v = JSON.parse(raw) as unknown;
    if (v && typeof v === "object" && !Array.isArray(v)) {
      return v as Record<string, unknown>;
    }
  } catch {
    /* truncated JSON */
  }
  return {};
}

export function isEditTool(name: string): boolean {
  const n = name.toLowerCase().replace(/_/g, "");
  return n === "write" || n === "strreplace" || n === "edit" || n === "delete";
}
