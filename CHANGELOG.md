# Changelog

未发版之前以 git log 为准。下面记用户能看见的行为变化。

- 跟进轮对齐 Cursor / grok CLI：compact 优先留下 Write/StrReplace 过的文件，不再只留 Glob/Read 头尾；`[history]` 默认只带本场归档，邻座会话要用户问起才贴；控制台撞步数/时间墙也留一跳收口，方便下一轮接着写。
- 巡检门对齐 Cursor / grok CLI：`tools[]` 中途不卸。连续只读只催原生 Write，再只读则跳过并回 `[already observed]`，下一跳仍可 Write。泄露旁白不当终稿，也不进 `[history]` / Decisions。步数墙才是硬停。
- 空跳不再当成功结束：无正文、无工具、无 usage 先重试一次；仍空则收口一跳，再空则给「没有可见回复」兜底（控制台也会，不再交空白气泡）
- Glob / Shell 只拦工作区根上的无过滤递归扫；`**/*` 给一份顶层样本（跳过 vendor / 嵌套 `release` / `out` / `unpacked`），不再空手训一通。带扩展名的会真扫，满 200 条截断；`target_directory` 指到文件时直接报错
- Grep 默认不扫打包目录（显式 `path` 进该目录仍可搜）。重复 / 近义 pattern 会折；目录级风暴上限 12 次，与 Search 是否挂上无关；单文件 Grep 不占名额
- 连续 3 跳低新意，或连续 10 跳只巡检（Read/Grep/Glob…，Write 会重置），催原生 Write；再只读则跳过并回 `[already observed]`，`tools[]` 不卸
- 思考撞上限：软提醒后按原推理模式给一次更宽重试，不关 thinking。空正文先收口一跳（工具仍在），再空则「没有可见回复」
- ReadLints：超时 / 没跑 checker 不再报「没有编译错误」；超时也不再标成工具 Error，会说明「不是编译结论」。tsc 从文件向上找最近的 tsconfig.json（例如 `web/console`）
- `view` 未挂进 tools[] 时幻觉调用返回 unknown；Read 图片/音视频会直接加载，不再指向未挂载的 view
- `ComputerUse` 未进冻结 17（`features.computer_use` 未开）时幻觉调用返回 unknown，不再真去控桌面；Ask 也不会先弹审批
- Write schema 要求 `contents`；EditNotebook schema 要求 `target_notebook`
- TodoWrite / ReadLints 标为可并行，不再把同一跳后面的 Read/Grep 拖成串行
- `auto` 对 grok-4.6 的文档改为 **xhigh**（与 `Effort::auto_for` 一致）
- 频道页只列出进程内适配器（webhook / Telegram / QQ / 微信 / 企微 / 钉钉 / 飞书）
- Electron：`hyper web` 退出后退避再拉起
- WS `Lagged` 的 `resync` 带当前会话 events，直播不再先缺一截
- QQ「还在处理中」在回合结束后立刻停
- 控制台「文件」页修回 `../` 路径穿越防护（读围栏放开后 `safe_join` 误走未受限的 `resolve`，读写都可逃逸目录根）
- IM 长回复不再截断：QQ / Telegram / 微信 / 企微 / 钉钉 / 飞书按自然边界分成多条气泡，代码块跨段自动闭合、下一泡重开
- IM 入站防抖按渠道分档（微信 3s、QQ 1s、飞书 / Telegram / 企微 / 钉钉 0.6s、飞书带媒体 0.8s），手机上连发多条不再被切成两轮
- IM 正文流式预览：回答生成期间，飞书 / Telegram / 企微的进度气泡同步「回复中」草稿（12s 节奏），QQ / 微信 / 钉钉随攒批消息发出
- 整段回复 `NO_REPLY` / `SILENT` 时不外发（Hermes 沉默 token）；群聊消息带 `[发言者]` 前缀进上下文，模型能分清谁在说话
- `/stop`、`/steer`、`/queue` 等斜杠指令不再走入站防抖，立即到达会话；同窗口「帮我改」+ /stop 不会再被 merge 成一段正文
- 沉默 token 不再从「回复中」预览漏出：流式期间命中 NO_REPLY / SILENT 前缀就压住草稿段，终稿拦截也认 `NO_REPLY。` 这类尾标点
- 飞书 / Telegram / 企微终稿发出后，旧进度泡收敛回工具摘要（没走工具则收回 ACK），长回答不再上下看两遍；QQ / 微信 / 钉钉的短回答不再提前播「回复中」草稿（≥400 字才播）
- 微信群消息以 @ 开头、企微群消息被 @ 时标注 is_mentioned，默认 mention 策略下机器人不再装死；微信长文拆成多次 sendmessage 真分泡；QQ 媒体兜底不再复用已用掉的被动 msg_id；企微只回图时不再发「…」
- 中文长回答按句读（。！？；，、：）切泡，不再从无空格句子中间硬断；正文里行内提到 ``` 不再被误判成围栏开闭
- IM 卡的定位指引改为 Grep（默认冻结 17 没有 Search），不再教模型幻觉调用未挂载的工具
- Read 目录：先排序再截断；坏 outbox JSON 进 quarantine；频道 poll lock 显示「重试中」
- IM 群聊会话改为 per-user（可带 Telegram topic / 飞书 thread）；运行中 `/stop` `/approvals` `/plan` 等只有发起者能改
- `hyper web` 各 IM endpoint 共用一个 SessionRouter / ChannelManager，跨平台 resume 同一 JSONL 不会跑两套 agent
- AskQuestion / 审批按 prompt id 排队（FIFO、15 分钟 TTL）；过期按钮会提示，飞书 / Telegram 选完后收回卡片
- IM 增加 `/model` `/compact` `/undo` `/usage`；`/new <title>` 会真正写入会话标题
- IM `/background`（`/bg` `/btw`）把当前任务留在原会话继续跑，本聊天切到新会话；要停后台：`/resume <id>` 再 `/stop`
- AskQuestion / 审批卡片选完后标明选择者，并保留原问题
- QQ 键盘、钉钉 actionCard、企微 template_card、webhook `choices` 走原生按钮（微信 iLink 仍用序号回复）
