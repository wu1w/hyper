# 技术说明

本文描述 grok-hyper（仓库 / CLI 名 `hyper`）**当前实现**。

仓库从 Qwenthin / q-harness 产品壳 fork。crate 已改名为 `hyper-*`，配置目录是 `~/.grok-hyper/`，工作区 overlay 是 `.grok-hyper/`。

三路传输：`grok login` → `cli-chat-proxy` Responses；`XAI_API_KEY` / 配置 key → `api.x.ai` Responses；自定义 `base_url` → 现有 Chat Completions。默认模型 `grok-4.6`，窗 500k，compact 80%。模型可见工具名对齐 Cursor，并包含 `ReadLints` / `EditNotebook` / `GenerateImage` / `SwitchMode`；配置 MCP 后使用 `GetDynamicTools` / `CallDynamicTool` / `FetchMcpResource` 运行时发现与调用。子代理深度 1，不 `exec grok`。默认 isolation 共用父 cwd；显式 `worktree` 才隔离。

## 它是什么

一个本机 grok-4.6 harness：Cursor 形 `tools[]`、可观测轨迹、全栈 rust。控制台有办公入口（频道、定时、文稿目录），编码和工作区编辑是同一套 agent。对话页右侧结果区有工作区树，Write/StrReplace 后会打开该文件预览；编辑结果可带 `[diagnostics]`，Search 对标识符走定义跨度。

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
plugins/vscode-hyper       可选 VS Code / Cursor 扩展（侧栏 Chat + vscode.diff；loop 仍是 sidecar）
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

`Read` · `Write` · `StrReplace` · `Delete` · `Glob` · `Grep` · `ReadLints` · `EditNotebook` · `Shell` · `WebSearch` · `WebFetch` · `GenerateImage` · `TodoWrite` · `AskQuestion` · `SwitchMode` · `Task` · `AwaitShell`

执行层仍接受 Qwen 四件套别名（`read` / `write` / `edit` / `bash`），发给模型的 `function.name` 必须是上表。`Read` 一个目录时先收集名称、排序，再截到 200 条（readdir 顺序下先截断会给出任意子集）。`ToolLimits::default()` 与 `[tools] read_default_lines = 600` 一致。

**按配置追加：**

| 工具 | 何时出现 |
|---|---|
| `web` | `[web] enabled`（默认开；无 key 走 Bing/DuckDuckGo HTML + 抓取） |
| `GetDynamicTools` + `CallDynamicTool` + `FetchMcpResource` | 配置里列出了 MCP server；发现 live `tools/list`、调用工具、列出/读取 resources（`downloadPath` 写入工作区） |
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

`isolation`：`none` 和 `auto`（默认；空字符串也是 auto）共用父 cwd，所以办公目录即使碰巧是 git 仓库，孩子看到的也是用户正在看的未提交稿，Write 也写回那里。`worktree` 必须是 git 仓库，从 **HEAD** 检出到 `~/.grok-hyper/worktrees/<child-id>`（看不见未提交改动），跑完**不拆树**、不 merge；SUMMARY 带 `WORKTREE` 绝对路径。`resume` 用 ChildRecord 里记下的 isolation，漏参数不会掉回 auto。下次 Task 只 prune 崩溃残留（没有 keep 标记的目录）。子代理 registry 写 `{id}.task.json`：进程重启后 `get_or_load` / AwaitShell 能从磁盘找到孩子；当时还在跑的记成 `interrupted: process restarted`，不自动重跑。schema 以外的 isolation 值直接报错。

孩子接到父的 `permit` / `clarify` / live sink。父是 ask 时，孩子写文件、跑 Shell、AskQuestion 走同一套收件箱，不会 YOLO，也不会报「需要 interactive channel」。孩子不把思考 / 正文 delta 混进父气泡；完成的工具事件会转发到父 live sink。`/fork --worktree` 仍然只拷会话，不建 git worktree。

## agent轨迹

1. 用户消息（可带图片等 `content_parts`）进入 mailbox。默认 **steer**：运行中发来的普通消息会在当前工具结束、下一个工具尚未启动的安全边界注入；同批尚未启动的串行工具会收到成对的 `skipped` 结果。`/queue` 明确排到当前轮之后，`/stop` 立即 cancel，`/busy steer|queue|interrupt` 可随时切换。AskQuestion 或审批挂起时，下一条发起者回复优先作为控件答案，不会进入 steer；飞书交互卡片和 Telegram inline keyboard 的点击与 `1/2/3`、选项 ID、`/skip`、自由文本一样进 `interaction::answer`。IM 审批默认 **ask**（与控制台/TUI 相同）；`/approvals yolo` 才跳过。`/plan`、`/clarify`、`/approvals` 以及「始终允许」写入 `~/.grok-hyper/sessions/<id>.controls.json`，`hyper web` 重启后仍在。每轮写入带稳定 `run_id / turn_id / step_id / tool_call_id` 的生命周期事件，Web 与 IM 都消费这套状态，而不是从文本猜「思考/工具/重试」。IM 立刻 ACK（「收到，正在处理…」），把思考 delta / 工具状态聚成少量聊天；正文 content delta 以「回复中」草稿预览（可编辑渠道 12s 节奏原地刷新，QQ / 微信 / 钉钉随攒批消息，终稿前不重复倾倒）；终稿超长时按平台上限分泡（自然边界切割，fence 跨泡闭合重开），不再截断；整段 `NO_REPLY` / `SILENT` 视为有意沉默，不外发；群消息注入 `[发言者]` 前缀；入站防抖按渠道 0.3–3s（微信最宽，飞书带媒体 0.8s）。Telegram / QQ C2C / 微信 iLink 发 typing；飞书给入站消息贴 `Typing` 反应，结束时摘掉；企业微信 ACK/思考走 `msgtype: stream` 同一气泡，终稿 `finish=true`。终稿先写 durable outbox，平台确认后转 receipt；重启会重放未确认消息，带入站消息 ID 的平台使用稳定幂等键。循环仍是满血编码 agent（xhigh、稳定 Cursor-shaped 工具面、500 步、30 分钟）。
2. `SidecarSession` 组 messages：角色边界（只读 `~/.grok-hyper/AGENT.md`，工作区 USER.md/SOUL.md 不能改人设）+ 可选工作区 `AGENTS.md`（项目约定）+ 冻结 tools + 历史。
3. HTTP 补全；思考 / 正文分通道流式推到 WS。
4. 工具调用按审批模式停或放行；结果写回 messages，直到模型停或打到 `max_steps`。
5. 事件追加到会话 JSONL；`stop` 结束本轮。WS 广播环只推增量；客户端 `Lagged` 时 **这条 socket** 收到 `resync`（state / permit / clarify / 当前会话 `console_events`，与 hello 相同、已去掉 inline `data:`），立刻重绘，不必等 80ms 的 `GET /history`。不要把整份 JSONL 丢回广播总线（Windows 上会卡死）。

默认 `auto` 对 grok-4.6 映射为 **xhigh**（思考关不掉）；Qwen 仍是官方中性 `medium`。`/think`、`--think`、`/fast` 仍可人工覆盖。grok 走 Responses：不回放思考、不回放 tool-hop 助手正文、不把 QwenPaw 的 `[trajectory]` / `[style]` / `[out]` / `[locate]` / `[oracle]` / `[guard]` 注记当用户消息。同参工具第 6 次安静停止（`budget:repeat`）。控制台和 TUI 不把 tool-hop 旁白画成答案气泡。Qwen 本地权重仍走软干预：同参 6 次提醒一次，dump 延后工具并观察一次。

轨迹控制：测试转红、修改测试期望和编辑摇摆只作为隐藏事实反馈，不替模型决定停止或回退。思考触及上限时保留模型选择的思考模式；grok 不再追加“collapse to one conclusion”讲义。只有再次触顶、时间、步数或上下文硬上限才终止。控制台/TUI 默认 **500 步**、30 分钟硬墙钟（与 IM / Hermes `max_turns` 对齐）。IM 默认 500 步、**30 分钟墙钟**（`max_wall_unattended_seconds = 1800`）；Shell 未带 `block_until_ms` 时由 coordinator（默认 `code_mode.timeout_s = 60`）offload/取消，bash 内层不再套 120 秒硬杀。出站空正文不发占位句；连接失败会重试，读超时不重试以免 QQ 重复消息。微信 iLink 长轮询独占 cursor，不能和 Hermes weixin 共用同一个 bot。子代理 `Task` 的 registry 写 `{id}.task.json`：进程重启后 `resume` / AwaitShell 能找到孩子；当时还在跑的记成 `interrupted: process restarted`，不自动重跑。

上下文窗口默认 **500000**。超过 `working_window * 0.80` 或 200k 价格悬崖时，session/api_key 走官方 `POST /v1/responses/compact`；openai_compat 仍用本地 archive compact。

## 会话与状态

- 会话文件：`~/.grok-hyper/sessions/<id>.jsonl`
- IM 控件：`~/.grok-hyper/sessions/<id>.controls.json`（plan / clarify / approvals / always-allow）
- 配置：`~/.grok-hyper/config.toml`（`hyper web` 只认这个文件，不认 `HYPER_*`）
- probe：`~/.grok-hyper/probe.json`
- 控制台上次工作区：`[console] workspace`

Web 启动时：若 CLI 没传 `--workspace`，用配置里的路径；路径不存在则退回进程 cwd。打开已有会话时 `bind_store` 以 JSONL `session/start.workspace` 为准。在控制台换目录会同时写 `[console] workspace` **和** 当前会话的 `session/start`；`hyper web --workspace PATH` 也会把该路径写入正在打开的会话。只改配置或只改内存、不改 JSONL 时，重启 / 恢复会话会回到旧目录。换目录只在**焦点**会话 `turn_in_flight` 时 409；停放在侧栏的会话不挡切换，它们若还在跑，Write 仍可能打进旧树。

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

对话页自己做 Markdown（表格、代码高亮、一部分图），不额外拉 npm 渲染库。流式时未闭合的围栏先当代码，闭合后再当图。活动条里 StrReplace / Write 展开为行级 diff（红删绿增），路径可点开右侧预览。右侧工作区树会标出本轮改过的文件。

前端让grok+kimi k3+fable5调了三轮美化过，成本巨大，各位老爷麻烦看到的点个星星，感激不尽。

## 频道与插件

`hyper-loop::channel` 把 QQ / 飞书 / Telegram / 微信 / 企微 / 钉钉 / webhook 收成同一套 mailbox（目录里只有这些 `in_process` 入口）。凭证在配置里，控制台 **频道** 页编辑。适配器任务退出、panic 或 poll lock 冲突后会指数退避再拉起，pill 显示「重试中」而不是「连接错误」。`hyper web` 与 `hyper --channels` 对同一 bot 互斥（poll lock）。无头进程级保活见 `contrib/systemd/hyper-web.service`（请用 **user unit**，不要以 root 原样 enable）。桌面 Electron 在 `hyper web` 退出后会退避再拉起。IM 进度走 `EventSink` 聚合，不改 `drive()` / 工具 JSON / xhigh。终稿先写 durable outbox；坏掉的 pending JSON 进 `quarantine/`，不再永远 skip。

`hyper --sidecar` 是 newline JSON-RPC（stdio）。dsh 插件和 VS Code 扩展只翻译 UI 事件，**禁止**再开一套工具循环。见 `plugins/dsh-plugin-hyper/README.md`、`plugins/vscode-hyper/README.md`。这不是 Cursor.app 那种 VS Code fork：agent 可以住在编辑器侧栏里，工具循环仍是同一份 `hyper --sidecar`。安装：`hyper vscode-install`。

每轮用户消息会带一份 Cursor 形感知卡：`[workset]`（日期、OS、打开的文件/选区、git、本轮窗口路径）和 `[rules]`（`.cursor/rules` / `.cursorrules` 里 alwaysApply 或 glob 命中的正文）。这不改会话内稳定的工具 JSON，也不改 `drive()`。完全复刻 Cursor 私有 system / 云端索引 / Apply 模型做不到。

## 为什么做这个玩意儿

- 我平时习惯用Hermes接本地模型，这次qwen3.8 27B的体验很不好， 超长 system + 全量 tool 动物园扣在 27B 上，加上模型爱思考，难用的一匹。
- dsh和pi agent的思路我很喜欢，但pi太简陋了，dsh折腾，原版agent给DS优化的。
- 阿里的几个agent难用，尤其是qoder，是史。我还试了qwenpaw接qwen3.8 27B，超过80轮工具的大会话harness会挂掉，再起不能，明显没对他家自己的本地模型做过优化适配。
