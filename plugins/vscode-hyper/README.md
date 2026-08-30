# vscode-hyper

VS Code / Cursor 扩展：侧栏对话，编辑器里看 diff。工具循环仍是 `hyper --sidecar`，**禁止**在这个包里再写一套 agent。

Cursor 的做法是 agent 住在 IDE 里。这里不 fork VS Code，只把同一套 Hyper 接到编辑器。

```
VS Code / Cursor
  侧栏 Chat webview + vscode.diff
        │
        ▼
vscode-hyper          （翻译 UI，不跑工具）
        │  stdio JSON-RPC
        ▼
hyper --sidecar       （tools / ThinkPolicy / JSONL）
```

## 安装

仓库根目录：

```
hyper vscode-install
```

会编译扩展并拷到 `~/.vscode/extensions/wu1w.hyper-0.1.0`。若存在 `~/.cursor`，也会装到 `~/.cursor/extensions`。然后 **Reload Window**。

需要 `node` 来编译扩展。安装器会把当前 `hyper` 绝对路径写进扩展目录的 `hyper.bin`，这样从 Dock 打开的编辑器也能找到二进制。仍可用设置 `hyper.command` 或环境变量 `HYPER_BIN` 覆盖。

## 用法

1. 用 VS Code 或 Cursor 打开工作区文件夹。
2. 活动栏 Hyper 图标 → Chat。
3. Write / StrReplace 完成后，编辑器打开 `vscode.diff`（改前快照 vs 磁盘上的新文件）。发送时会把当前打开的文件、光标行和选区交给 sidecar，写进 `[workset]`。

模型、key、会话 JSONL 仍走 `~/.grok-hyper/`。
