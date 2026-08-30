# grok-hyper

grok-4.6 的 Cursor 形 agent harness。CLI 二进制是 `hyper`，配置在 `~/.grok-hyper/`。

默认模型 **grok-4.6**。`grok login` 会话和 xAI API key 走 Responses API；自定义 `base_url` 走 OpenAI 兼容 Chat Completions。发给模型的工具名对齐 Cursor：Read / Write / StrReplace / Delete / Glob / Grep / ReadLints / EditNotebook / Shell / WebSearch / WebFetch / GenerateImage / TodoWrite / AskQuestion / SwitchMode / Task / AwaitShell。配置 MCP 后额外挂载 `GetDynamicTools` / `CallDynamicTool` / `FetchMcpResource`，server 工具在运行时发现，不再暴露单个 `mcp` blob。

`Task` 默认 `isolation=auto`（空也是 auto），和 `none` 一样共用父工作区：未提交的文稿和孩子的 Write 都还在用户正在看的目录。只有显式 `isolation=worktree` 才建 git worktree（从当前 HEAD 检出，看不见未提交改动）；跑完目录留在 `~/.grok-hyper/worktrees/<id>`，SUMMARY 里带 `WORKTREE` 路径，resume 用当时记下的 isolation（漏参数不会掉回 auto）。崩溃且没写完 keep 标记的目录，下次 Task 会 prune。孩子走父的审批 / AskQuestion 通道。`/fork --worktree` 仍然只拷会话，不建 git worktree。

控制台侧栏仍按「办公 / 工作区」分组（频道、定时、文稿快捷方式），那是入口信息架构，不是「这不是 coding agent」。

更细的内部结构见 [技术说明](docs/architecture.md)。

## 需要什么

- Rust 工具链（`rustup`）
- `grok login`、或 `XAI_API_KEY` / 控制台粘贴的 key、或任意 OpenAI 兼容端点
- Windows 用户尽量安装 Git Bash 和 PowerShell 7

## 三步跑起来

```bash
# 1. 编译安装
cargo install --path crates/hyper-cli
# 或: cargo build --release && 把 target/release/hyper 放到 PATH

# 2. 三选一
grok login                          # 本机会话 → cli-chat-proxy Responses
export XAI_API_KEY=xai-...          # → api.x.ai Responses
# 或在控制台「模型」页填自定义 OpenAI 端点

# 3. 打开控制台
hyper web
```

浏览器会打开 `http://127.0.0.1:3848/`。也可在控制台 **模型** 页选接入方式（`grok login` 会话 / API key / 自定义 OpenAI 端点），模型名默认 `grok-4.6`。

`hyper web --bind 127.0.0.1:3848 --no-open` 只起服务、不弹浏览器。无头常驻用 `contrib/systemd/hyper-web.service`（`Restart=always`）。`hyper web` 已经带频道 poll，不要再叠一层 `hyper --channels`。

确实要在可信局域网访问控制台时，显式使用 `hyper web --bind 0.0.0.0:3848 --allow-lan`。

## 控制台怎么用

侧栏两组，办公在前：

| 分组 | 页 | 做什么 |
|---|---|---|
| 办公 | 聊天 | 主对话。AskQuestion 选择题、TodoWrite 清单、工具轨迹 |
| 办公 | 频道 | QQ / 飞书 / Telegram 等外部入口 |
| 办公 | 文件 | 文稿 / 下载 / 桌面等办公文件夹，不是代码仓库浏览器 |
| 办公 | 定时任务 | 到点向当前文件夹发一轮（主机定时器，不是模型工具） |
| 办公 | 心跳 | 周期性巡检提示 |
| 工作区 | 收件箱 | AskQuestion 入口提示 + 工具审批（ask 模式下写文件、跑命令会停在这里） |
| 工作区 | 会话 | 历史 JSONL、切换、删除 |
| 工作区 | 技能 | `SKILL.md` 目录 |
| 工作区 | MCP | 外部 MCP 进程 |
| 工作区 | 工具 | 稳定 tools[]。名字对齐 Cursor（含 ReadLints / EditNotebook / GenerateImage / SwitchMode）；MCP 使用 GetDynamicTools / CallDynamicTool / FetchMcpResource |
| 底栏 | 模型 | 三路接入、模型名、窗口、步数 |
| 底栏 | 安全 | 审批档位 |
| 底栏 | 用量 | token / 缓存命中 / 200k 价格悬崖 |

文件页可以粘贴绝对路径或 `~/…`，或用系统选择 / 浏览。快捷方式指向主目录、桌面、文稿、下载。换目录会写入 `~/.grok-hyper/config.toml` 的 `[console] workspace`，以及当前会话 JSONL 的 `session/start.workspace`（否则重启 / 恢复会回到旧目录）。这里不做迷你 IDE：改文稿走聊天里的 Write / StrReplace。

默认 ask：写文件、跑命令会先问。可在聊天或安全页改成自动 / yolo。这是本机控制台，不是沙箱产品。

输入超过 200k token 后单价翻倍（`$2 / $0.50 / $6` → `$4 / $1 / $12`，每百万，输入 / 缓存命中 / 输出）。用量页和模型页会提示。

## 命令行

```bash
hyper web                         # 控制台（主入口）
hyper --print "总结这份文稿"       # 一次性跑完打到 stdout
hyper                             # 在 TTY 且没有 prompt 时开 TUI
hyper probe                       # 探测端点能力，写 probe.json
hyper --sidecar                   # stdio JSON-RPC（给可选 dsh / VS Code 插件用）
hyper vscode-install              # 可选：装 VS Code / Cursor 侧栏扩展
```

全局 `--workspace` 指定根目录。`--print` 适合脚本；日常请用 `hyper web`。

环境变量 `HYPER_BASE_URL` / `HYPER_API_KEY` / `HYPER_MODEL` **只覆盖 CLI/TUI**。`hyper web` 故意不读它们，避免设置页显示的和真正用的不一致。Web 模式请改控制台或 `config.toml`。

## 配置

路径：`~/.grok-hyper/config.toml`。字段说明和默认值见仓库里的 [`config.example.toml`](config.example.toml)。

常见项：

- `[server]` 端点、key、模型、引擎 profile、家族
- `[context] working_window` 默认 **500000**，soft compact 80%；超过 200k token 后 xAI 单价翻倍
- `[policy] default_effort = "auto"` 对 grok-4.6 映射为 **high**；`/think` 仍可覆盖
- `[console] workspace` 控制台上次选的文件夹
- `[web]` 搜索工具（无 key 也能用；有 Tavily key 自动升级）
- `[mcp]` / 技能目录 overlay

人设只认 `~/.grok-hyper/AGENT.md`。工作区 `USER.md` / `SOUL.md` / `AGENT.md` 当项目文档，不会改名字。工作区还可以放 `.grok-hyper/skills`、`.grok-hyper/mcp.toml`。

## 改控制台前端

`hyper web` 托管的是已经编好的 `web/console/dist`。改 React 之后：

```bash
cd web/console && npm install && npm run build
```

再重启 `hyper web`。开发时 `npm run dev` 会把 `/api` 代理到本机 3848。

## 可选：VS Code / Cursor

`hyper vscode-install` 把侧栏 Chat 接到编辑器，Write / StrReplace 用 `vscode.diff` 打开。工具循环仍是 `hyper --sidecar`，不是第二套 agent，也不是 fork VS Code。说明见 [`plugins/vscode-hyper/README.md`](plugins/vscode-hyper/README.md)。

## 可选：dsh 插件

已经在用 dsh 的人可以 `hyper dsh-install`，把同一套 loop 挂到 dsh 里。**产品壳仍是 `hyper web`**，不是 dsh。说明见 [`plugins/dsh-plugin-hyper/README.md`](plugins/dsh-plugin-hyper/README.md)。
