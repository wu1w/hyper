# Hyper 源码工作区

本仓库就是 harness。改自己前先读 `docs/dsh-playbook.md`，先打快照再动文件。

- 循环只在 `crates/hyper-loop`（`agent/turn.rs` 单一裁决）。不要在 web / dsh / vscode 插件里再开一套 loop。
- 配置样例 `config.example.toml`；机器上的活文件是 `~/.grok-hyper/config.toml`（只读绝对路径；写入要审批或拷回）。
- 会话 `~/.grok-hyper/sessions/<id>.jsonl`，官方 compact 旁路 `<id>.official.json`。
- 保险：`./scripts/self-snap.sh` 打 tag；文件回滚用 git；对话回滚用 `/undo`。二者不是一回事。
- 过夜墙：`max_steps` / `max_wall_*` 必须是 0。旧 500/1800 加载时会迁走。
- `/think` 是 100 步短推理，不要用来过夜。
