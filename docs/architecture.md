# 技术说明

本文描述 grok-hyper（仓库 / CLI 名 `hyper`）**当前实现**。

仓库从 Qwenthin / q-harness 产品壳 fork。crate 已改名为 `hyper-*`，配置目录是 `~/.grok-hyper/`，工作区 overlay 是 `.grok-hyper/`。

三路传输：`grok login` → `cli-chat-proxy` Responses；`XAI_API_KEY` / 配置 key → `api.x.ai` Responses；自定义 `base_url` → 现有 Chat Completions。默认模型 `grok-4.6`，窗 500k，compact 80%。模型可见工具名是 Cursor 的 `Read` / `Write` / `StrReplace` / `Shell` / `Grep` / `Glob` / `WebSearch` / `WebFetch` / `TodoWrite` / `AskQuestion` / `Task` / `AwaitShell`。子代理深度 1，不 `exec grok`。默认 isolation 共用父 cwd；显式 `worktree` 才隔离。

## 它是什么

一个本机 grok-4.6 harness：Cursor 形 `tools[]`、可观测轨迹、全栈 rust。控制台有办公入口（频道、定时、文稿目录），编码和工作区编辑是同一套 agent，不做 IDE 卖点。

主交付面是 **进程内 Web 控制台**（`hyper web`），外加 Electron 壳。

优化对象是 **grok-4.6 的 Cursor 训练分布**（工具名、并行、AskQuestion、TodoWrite、Task），不是再给 27B 瘦身。

## 仓库结构

```
crates/hyper-loop   循环、工具、会话 JSONL、sidecar RPC、频道、probe
crates/hyper-web    axum：静态控制台 + /api + WebSocket
crates/hyper-cli    hyper 可执行文件
crates/hyper-tui    终端 UI
crates/hyper-bench  小任务基准
web/console       Vite + React；产物在 dist/，由 hyper-web 托管
plugins/dsh-plugin-hyper   可选 stdio 客户端（不是产品壳）
config.example.toml      ~/.grok-hyper/config.toml 的字段说明
```

`hyper-loop` 的工具轨迹形状参考了下QwenPaw 的行为；家族请求构造和 probe 是我自己写的。

## 进程模型

```
浏览器  ──HTTP/WS──►  hyper web (hyper-web)
                         │
                         ▼
                   SidecarSession (hyper-loop)
                         │
                         ├── TransportCompleter
                         │     ├── ResponsesCompleter → /v1/responses（session / api_key）
                         │     └── HttpCompleter      → /v1/chat/completions（openai_compat）
                         ├── 工具            → 当前工作区磁盘 / Shell
                         └── 会话 JSONL      →  ~/.grok-hyper/sessions/
```

没有单独的 agent 守护进程。控制台、TUI、sidecar 都是同一个 session 对象上的 `turn.start`。

Cron / 心跳 / 频道入站也是 **主机定时器或适配器** 去调 `turn.start`，不会给模型多暴露一个 `cron` 工具。

## 工具面

会话开始时冻结一份 `tools[]`，中途不改 schema 字节（这块主要参考了dsh的玩法，提高缓存命中率，降低本地模型的负担）。

**日常冻结套件（Cursor 名，顺序固定）：**

`Read` · `Write` · `StrReplace` · `Delete` · `Glob` · `Grep` · `Shell` · `WebSearch` · `WebFetch` · `TodoWrite` · `AskQuestion` · `Task` · `AwaitShell`

执行层仍接受 Qwen 四件套别名（`read` / `write` / `edit` / `bash`），发给模型的 `function.name` 必须是上表。

**按配置追加：**

| 工具 | 何时出现 |
|---|---|
| `web` | `[web] enabled`（默认开；无 key 走 Bing/DuckDuckGo HTML + 抓取） |
| `mcp` | 配置里列出了 MCP server |
| `skill` | 技能目录存在时 |
| `view` | 打开图片 / 音视频 |
| `search` | 代码搜索 |
| `recall` / `memory_search` | compact 之后再挂上，避免改冻结四工具的 JSON |

`code` 模式是另一组：`run_code`、`read`、`bash`。

`search` 使用函数级 SQLite FTS 索引，只把命中的有限代码片段交给模型。Git 工作区的索引缓存在 `~/.grok-hyper/code-index/`，再次打开时按文件大小和纳秒 mtime 增量刷新；项目目录里不生成索引文件。工具写文件后会即时刷新对应条目。

`bash` 对模型保持一个名字和一种常用语法：macOS/Linux 走无 profile Bash，Windows 优先自动发现 Git Bash，没有时才退到无 profile PowerShell。可用 `HYPER_SHELL` 显式覆盖，但正常安装无需给模型增加操作系统判断提示。

模型若发 Qwen 风格的 XML `<tool_call>`，和 OpenAI `tool_calls` 走同一套解析合并。

工作区路径用 `Workspace` 做相对解析。控制台换根目录等于换这个 `Workspace`，并 `refresh_surface` 重载该目录下的技能 / MCP overlay。

## 子代理（Task）

深度 1：孩子不能再 `Task`。explore / plan 只读（plan 可写 `plan.md`）。office / generalPurpose 可写。

`isolation`：`none` 和 `auto`（默认；空字符串也是 auto）共用父 cwd，所以办公目录即使碰巧是 git 仓库，孩子看到的也是用户正在看的未提交稿，Write 也写回那里。`worktree` 必须是 git 仓库，从 **HEAD** 检出到 `~/.grok-hyper/worktrees/<child-id>`（看不见未提交改动），跑完**不拆树**、不 merge；SUMMARY 带 `WORKTREE` 绝对路径。`resume` 用 ChildRecord 里记下的 isolation，漏参数不会掉回 auto。下次 Task 只 prune 崩溃残留（没有 keep 标记的目录）。进程重启后内存 registry 没了，按 id resume 会找不到——那是另一回事。schema 以外的 isolation 值直接报错。

孩子接到父的 `permit` / `clarify` / live sink。父是 ask 时，孩子写文件、跑 Shell、AskQuestion 走同一套收件箱，不会 YOLO，也不会报「需要 interactive channel」。孩子不把思考 / 正文 delta 混进父气泡；完成的工具事件会转发到父 live sink。`/fork --worktree` 仍然只拷会话，不建 git worktree。

## agent轨迹

1. 用户消息（可带图片等 `content_parts`）进入 mailbox（忙时排队或打断，看 `[channels]` 的 busy 策略）。
2. `SidecarSession` 组 messages：角色边界 + 可选 `AGENT.md` + 冻结 tools + 历史。
3. HTTP 补全；思考 / 正文分通道流式推到 WS。
4. 工具调用按审批模式停或放行；结果写回 messages，直到模型停或打到 `max_steps`。
5. 事件追加到会话 JSONL；`stop` 结束本轮。控制台会重放本轮前半段，避免 WS 丢包后画面残缺。

默认 `auto` 对 grok-4.6 映射为 **high**（思考关不掉）；Qwen 仍是官方中性 `medium`。`/think`、`--think`、`/fast` 仍可人工覆盖。grok 走 Responses：不回放思考、不回放 tool-hop 助手正文、不把 QwenPaw 的 `[trajectory]` / `[style]` / `[out]` / `[locate]` / `[oracle]` / `[guard]` 注记当用户消息。同参工具第 6 次安静停止（`budget:repeat`）。控制台和 TUI 不把 tool-hop 旁白画成答案气泡。Qwen 本地权重仍走软干预：同参 6 次提醒一次，dump 延后工具并观察一次。

轨迹控制：测试转红、修改测试期望和编辑摇摆只作为隐藏事实反馈，不替模型决定停止或回退。思考触及上限时保留模型选择的思考模式；grok 不再追加“collapse to one conclusion”讲义。只有再次触顶、时间、步数或上下文硬上限才终止。控制台/TUI 默认 80 步、30 分钟硬墙钟。IM 默认 500 步、**10 分钟墙钟**（`max_wall_unattended_seconds = 600`）；Shell 未带 `block_until_ms` 时默认 120 秒。出站空正文会发「(无文本回复)」并重试 3 次。微信 iLink 长轮询独占 cursor，不能和 Hermes weixin 共用同一个 bot。子代理 `Task` 的 registry 写 `{id}.task.json`：进程重启后 `resume` / AwaitShell 能找到孩子；当时还在跑的记成 `interrupted: process restarted`，不自动重跑。

上下文窗口默认 **500000**。超过 `working_window * 0.80` 或 200k 价格悬崖时，session/api_key 走官方 `POST /v1/responses/compact`；openai_compat 仍用本地 archive compact。

## 会话与状态

- 会话文件：`~/.grok-hyper/sessions/<id>.jsonl`
- 配置：`~/.grok-hyper/config.toml`（`hyper web` 只认这个文件，不认 `HYPER_*`）
- probe：`~/.grok-hyper/probe.json`
- 控制台上次工作区：`[console] workspace`

Web 启动时：若 CLI 没传 `--workspace`，用配置里的路径；路径不存在则退回进程 cwd。

## HTTP API（本机）

控制台前缀 `/api`。和前端相关的是这些：

- `POST /api/rpc` sidecar 方法（`turn.start` / `session.*` / `slash` …）
- `GET /api/events` WebSocket（`state`、`event.append`、`history.replace`、`permit.ask`）
- `GET/POST /api/config` 模型与行为
- `GET /api/tree`、`GET /api/files` 工作区树和预览
- `GET/POST /api/workspace` 切换根目录；`POST /api/workspace/pick` 系统选文件夹；`GET /api/workspace/ls` 列子目录
- skills / mcp / channels / jobs / heartbeat / permit / usage

绑定默认 `127.0.0.1`，跨域响应只放行 loopback 开发页，WebSocket 校验同源。本机控制台优先好用，不做远程多租户安全模型。非 loopback 绑定必须显式传 `--allow-lan`，含义是“我信任这个局域网”；换工作区仍可指向本机任意文件夹。

## 前端

`web/console` 是 SPA。`hyper-web` 只托管 `dist/`，运行时不需要 Node。

对话页自己做 Markdown（表格、代码高亮、一部分图），不额外拉 npm 渲染库。流式时未闭合的围栏先当代码，闭合后再当图。

前端让grok+kimi k3+fable5调了三轮美化过，成本巨大，各位老爷麻烦看到的点个星星，感激不尽。

## 频道与插件

`hyper-loop::channel` 把 QQ / 飞书 / Telegram 等收成同一套 mailbox。凭证在配置里，控制台 **频道** 页编辑。适配器任务退出或 panic 后会指数退避再拉起；`hyper web` 与 `hyper --channels` 对同一 bot 互斥（poll lock）。无头进程级保活见 `contrib/systemd/hyper-web.service`。

`hyper --sidecar` 是 newline JSON-RPC（stdio）。dsh 插件只翻译 UI 事件，**禁止**再开一套工具循环。见 `plugins/dsh-plugin-hyper/README.md`。

## 为什么做这个玩意儿

- 我平时习惯用Hermes接本地模型，这次qwen3.8 27B的体验很不好， 超长 system + 全量 tool 动物园扣在 27B 上，加上模型爱思考，难用的一匹。
- dsh和pi agent的思路我很喜欢，但pi太简陋了，dsh折腾，原版agent给DS优化的。
- 阿里的几个agent难用，尤其是qoder，是史。我还试了qwenpaw接qwen3.8 27B，超过80轮工具的大会话harness会挂掉，再起不能，明显没对他家自己的本地模型做过优化适配。
