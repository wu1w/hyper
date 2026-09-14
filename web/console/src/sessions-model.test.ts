import assert from "node:assert/strict";
import { allShownPicked, filterSessions, sessionChannels, togglePicked } from "./sessions-model.ts";
import type { SessionInfo } from "./api.ts";

const rows: SessionInfo[] = [
  { id: "s1", title: "登录页", preview: "改标题", channel: "console", mode: "agent" },
  { id: "s2", title: "QQ 机器人", preview: "扫码", channel: "qq", mode: "agent" },
  { id: "s3", title: "闲聊", preview: "hello", channel: "console", mode: "chat" },
];

assert.deepEqual(
  filterSessions(rows, "登录", "all").map((s) => s.id),
  ["s1"],
);
assert.deepEqual(
  filterSessions(rows, "", "qq").map((s) => s.id),
  ["s2"],
);
assert.deepEqual(
  filterSessions(rows, "s3", "console").map((s) => s.id),
  ["s3"],
);
assert.deepEqual(sessionChannels(rows).sort(), ["console", "qq"]);
assert.equal(allShownPicked(["s1", "s3"], new Set(["s1", "s3"])), true);
assert.equal(allShownPicked(["s1", "s3"], new Set(["s1"])), false);
assert.equal(allShownPicked([], new Set()), false);
assert.deepEqual([...togglePicked(new Set(["s1"]), "s2", true)].sort(), ["s1", "s2"]);
assert.deepEqual([...togglePicked(new Set(["s1"]), "s1", false)], []);
