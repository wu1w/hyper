# dsh 玩法：Hyper 改 Hyper

dsh 不是产品壳。产品壳是 `hyper web`。dsh 的价值是：**同一套 loop 看见自己的配置和源码，能改自己，改坏了能退回去。**

把 Hyper 仓库当工作区打开，模型用的就是这套工具（Read / Grep / Write / StrReplace / Shell / TodoWrite）。不要再开第二套 agent 循环。

## 开场

```
cd /path/to/hyper
hyper web
```

工作区指到本仓库。第一句对模型说：

> 按 docs/dsh-playbook.md 做自检。先 snapshot，再看配置和核心 harness，不要先改二进制。

或斜杠读 skill：提到「自己改自己 / 回滚 / dsh」时 harness 会挂 `hyper-self`。

## 模型先看什么（全部暴露）

| 层 | 路径 | 干什么 |
|---|---|---|
| 运行时配置 | `~/.grok-hyper/config.toml` | 墙、窗、超时、模型。出厂过夜：`max_steps=0` `max_wall_seconds=0` `read_timeout_s=0` |
| 字段说明 | `config.example.toml` | 每个键的含义 |
| 家目录 | `~/.grok-hyper/` | `AGENT.md`、`sessions/*.jsonl`、`*.official.json`、`cron.json`、skills |
| 工作区 overlay | `.grok-hyper/` | 本仓库 gitignore；inbox / HEARTBEAT.md / 本地 skills |
| 循环 | `crates/hyper-loop/src/agent/` | `turn.rs` 唯一裁决；`window.rs` compact；`dispatch.rs` 工具 |
| 门 | `crates/hyper-loop/src/paw_loop/gates/` | 只挂 Doom / Iteration / Timeout。0 = 不限 |
| 会话 | `crates/hyper-loop/src/session/` | JSONL、compact、官方 blob |
| 传输 | `crates/hyper-loop/src/llm_http.rs` `agent/responses.rs` | 流式、keepalive、官方 compact skip |
| 心跳 | `crates/hyper-loop/src/cron.rs` `crates/hyper-web/src/hub.rs` | pulse 不持锁；树没变且无 `/loop` prompt 不叫醒。HEARTBEAT.md 只进指纹与文案 |
| 壳 | `crates/hyper-web/` `web/console/` `crates/hyper-cli/` | 控制台 / 静态页 / 可执行文件 |
| 可选壳 | `plugins/dsh-plugin-hyper/` | 只翻译 UI，禁止第二套工具循环 |
| 架构 | `docs/architecture.md` | 单一裁决、冻结 tools[] |

只读顺序：`docs/architecture.md` → `config.toml` → `agent/turn.rs` 的 `adjudicate` → `window.rs` 的 `compact_if_needed` → `paw_loop/gates`。

不要把 `~/.grok-hyper/sessions/*.jsonl` 当源码改。那是轨迹，不是组件。

## 怎么改自己

1. **先保险。** `scripts/hyper-self-snapshot.sh`（配置副本 + 工作区 `git stash create` 记一个点）。
2. **只改一处。** 长任务问题在门和窗口，不要同时动 UI。
3. **用工具写文件，不要口述。** Write / StrReplace 落盘；改 `config.toml` 前再拷一份到 snapshot 目录。
4. **验证。** `cargo test -p hyper-loop --lib <名字>` 跑相关单测。改配置后重启 `hyper web`。
5. **不要覆盖正在跑的二进制。** 先 `cargo build -p hyper-cli`，再停 web、换二进制、启动。

实验用 `/fork` 复制会话（JSONL 分叉，`--worktree` 现在忽略）。代码隔离用 `git worktree` 或本脚本的 stash 点。

## 保险与回滚

| 想退什么 | 怎么退 |
|---|---|
| 上一句用户 / 刚跑坏的一轮 | `/undo` 或 `/rewind`；`/retry` = undo + 重发 |
| 刚改的源码 | `scripts/hyper-self-rollback.sh` 或 `git checkout -- <file>` / `git reset --hard <snapshot>` |
| 刚改的配置 | `cp ~/.grok-hyper/snapshots/<id>/config.toml ~/.grok-hyper/config.toml`，重启 web |
| 官方 compact blob | 快照里的 `*.official.json`；不要只删 sidecar |
| 整次实验 | 扔掉 git worktree，或 `git reset --hard` 到 snapshot 记录的 HEAD |

规则：

- 没有 snapshot 不准改 `config.toml` 和 `turn.rs` / `window.rs` / `gates/`。
- `git reset --hard` 只在 snapshot 之后、且用户明确说可以丢工作区时用。
- 不要 `cargo install --force` 盖掉正在跑的 `hyper`。
- 不要把 `max_steps` / `max_wall_seconds` 改回 500 / 1800。加载器会把这两个出厂值迁成 0；自定义帽会保留。

## 自检清单（模型按这个找问题）

过夜 / 早停 / 空转：

- `config.toml` 是否还是 500 / 1800（应被迁成 0；若仍停，看是不是自定义帽）
- `turn.rs` `adjudicate` 是否又多了一条 Stop
- `DoomLoopGate::grok_default` 是否又变成 halt
- `compact_if_needed` 是否又 `budget:context` 收轮
- `prefix_tokens_gate` 有官方 blob 时是否按 blob+后缀计价
- `workspace_pulse` 是否把 `.` / `.grok-hyper` 目录 mtime 算进指纹
- `llm_http` 是否又给整段请求加了 `.timeout(1800)`
- AwaitShell 超时必须是 `Interrupted`，不能当 Success

改完用一句话写进 session：改了哪个文件、snapshot id、怎么回滚。
