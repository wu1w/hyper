# 控制台前端

Vite + React。办公产品线：聊天 / 频道 / 文件 / 定时 / 心跳，以及收件箱、模型三路接入、用量 200k 提示。

默认 grok-4.6。会话 / API key 走 Responses，自定义端点走 Chat Completions。`hyper web` 托管的是这里的 `dist/`。改完 UI 后：

```bash
npm install
npm run build
```

`npm run dev` 把 `/api` 代理到 `http://127.0.0.1:3848`（可用 `HYPER_WEB` 改）。

产品怎么用见仓库根目录 [README](../../README.md)，内部结构见 [技术说明](../../docs/architecture.md)。
