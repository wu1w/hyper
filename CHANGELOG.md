# Changelog

未发版之前以 git log 为准。下面记用户能看见的行为变化。

## Unreleased

- `auto` 对 grok-4.6 的文档改为 **xhigh**（与 `Effort::auto_for` 一致）
- 频道页只列出进程内适配器（webhook / Telegram / QQ / 微信 / 企微 / 钉钉 / 飞书）
- Electron：`hyper web` 退出后退避再拉起
- WS `Lagged` 的 `resync` 带当前会话 events，直播不再先缺一截
- QQ「还在处理中」在回合结束后立刻停
- Read 目录：先排序再截断；坏 outbox JSON 进 quarantine；频道 poll lock 显示「重试中」
