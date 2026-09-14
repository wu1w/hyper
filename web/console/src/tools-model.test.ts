import assert from "node:assert/strict";
import { CURSOR_TOOLS, onOffPill, webEnabledOf, webEngineLabel, webProviderOf } from "./tools-model.ts";

assert.ok(CURSOR_TOOLS.includes("Read"));
assert.ok(CURSOR_TOOLS.includes("AwaitShell"));
assert.equal(webProviderOf("search (provider: tavily)", null), "tavily");
assert.equal(webProviderOf("search", { provider: "tavily" }), "tavily");
assert.equal(webProviderOf("search", { tavily_key_set: true }), "tavily");
assert.equal(webProviderOf("", null), "builtin");
assert.equal(webEnabledOf({ enabled: false }, true), false);
assert.equal(webEnabledOf(null, true), true);
assert.equal(webEnabledOf(null, false), false);
assert.equal(webEngineLabel({ tavily_key_set: true }), "Tavily（已配 key）");
assert.equal(webEngineLabel({}), "内置（Bing / DuckDuckGo，免配置）");
assert.deepEqual(onOffPill(true), { cls: "ok", label: "已启用" });
assert.deepEqual(onOffPill(false), { cls: "idle", label: "未启用" });
