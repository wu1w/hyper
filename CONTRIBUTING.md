# 给贡献者

日常入口是 `hyper web`，配置在 `~/.grok-hyper/`。不要改维护者本机的 `config.toml`。

## 构建与测试

```bash
cargo test --workspace --locked
cd web/console && npm ci && npm test && npm run build
```

CI 跑上面这两套。标了 `#[ignore]` 的 live 套件（llama.cpp、真实 IM、网络 soak）**没有** GitHub job，需要密钥和长跑，只在本机按注释跑：

```bash
cargo test -p hyper-loop -- --ignored
```

不要实现 Discord / Slack / iMessage 等目录里没有适配器的频道。冻结的 Cursor `tools[]` 不要加 `recall` / `memory_search`。

## 改控制台

改 `web/console/src/`，再 `npm run build` 更新 `dist/`（`hyper web` 托管这份产物）。

## 提 issue / PR

- bug：复现步骤、期望、实际、`hyper --version`、OS
- 功能：先对现有架构（`docs/architecture.md`），不要平行再开一套工具循环
