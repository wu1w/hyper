import * as vscode from "vscode";

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewId = "hyper.chat";
  private view?: vscode.WebviewView;
  private pending: Record<string, unknown>[] = [];

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly onMessage: (msg: Record<string, unknown>) => void,
  ) {}

  resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;
    const webview = webviewView.webview;
    webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "media")],
    };
    const script = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, "media", "chat.js"));
    const style = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, "media", "chat.css"));
    const nonce = String(Date.now());
    webview.html = `<!DOCTYPE html>
<html lang="zh">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';" />
  <link rel="stylesheet" href="${style}" />
</head>
<body>
  <div id="log" class="log"></div>
  <div class="composer">
    <textarea id="input" rows="3" placeholder="给 Hyper 发消息… Enter 发送，Shift+Enter 换行"></textarea>
    <div class="row">
      <span id="status" class="status">未连接</span>
      <button type="button" id="abort">停止</button>
      <button type="button" id="send">发送</button>
    </div>
  </div>
  <script nonce="${nonce}" src="${script}"></script>
</body>
</html>`;
    webview.onDidReceiveMessage((msg) => {
      if (msg && typeof msg === "object") {
        this.onMessage(msg as Record<string, unknown>);
      }
    });
    webviewView.onDidDispose(() => {
      if (this.view === webviewView) {
        this.view = undefined;
      }
    });
    for (const m of this.pending) {
      void webview.postMessage(m);
    }
    this.pending = [];
  }

  post(msg: Record<string, unknown>): void {
    if (!this.view) {
      this.pending.push(msg);
      return;
    }
    void this.view.webview.postMessage(msg);
  }
}
