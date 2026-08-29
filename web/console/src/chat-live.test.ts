import assert from "node:assert/strict";
import {
  applyHistoryIncoming,
  coversLive,
  isPrepareHint,
  lastAssistantContent,
  lastAssistantInCurrentTurn,
  preferFresherHistory,
  stripLeakedToolMarkup,
  stripThinkRestatement,
} from "./chat-live.ts";
import type { SessionEvent } from "./api.ts";

const user: SessionEvent = { type: "user", text: "把登录页标题改成 ixiaotao" };
const asst: SessionEvent = { type: "assistant", content: "已改好标题。" };

assert.equal(lastAssistantContent([user, asst]), "已改好标题。");
assert.equal(lastAssistantContent([user]), "");
assert.equal(lastAssistantInCurrentTurn([user, asst]), "已改好标题。");
assert.equal(lastAssistantInCurrentTurn([user]), "");

{
  const hop: SessionEvent = {
    type: "assistant",
    content: "先登中转机看配置。",
    tool_calls: [{ id: "c1", function: { name: "Read", arguments: "{\"path\":\"a.rs\"}" } }],
  };
  assert.equal(lastAssistantContent([user, hop]), "");
  assert.equal(lastAssistantInCurrentTurn([user, hop]), "");
  assert.equal(coversLive([user, hop], { think: "", content: "已改好标题。" }), false);
}

{
  const live = { think: "", content: "已改好标题。" };
  assert.equal(coversLive([user, asst], live), true);
  assert.equal(coversLive([user], live), false);
  assert.equal(coversLive([user, asst], { think: "x", content: "" }), true);
  assert.equal(
    coversLive([user, { type: "assistant", content: "已改" }], { think: "", content: "已改好标题。" }),
    false,
  );
}

{
  const cur = [user, asst];
  assert.deepEqual(preferFresherHistory(cur, [user]), cur);
  assert.deepEqual(preferFresherHistory(cur, []), cur);
  const later = [user, { type: "assistant", content: "已改好标题。并保存。" }];
  assert.deepEqual(preferFresherHistory(cur, later), later);
}

{
  const hist = [user, { type: "assistant", content: "先确认上传文件是否存在。" }];
  assert.deepEqual(preferFresherHistory(hist, []), hist);
  assert.deepEqual(applyHistoryIncoming(hist, [], true), []);
  assert.deepEqual(applyHistoryIncoming(hist, [], false), hist);
}

{
  const t1u: SessionEvent = { type: "user", text: "先看 Cargo.toml" };
  const t1a: SessionEvent = { type: "assistant", content: "依赖已经列出来了。" };
  const t2u: SessionEvent = { type: "user", text: "把登录页标题改成 ixiaotao" };
  const prior = [t1u, t1a, t2u];
  const live = { think: "", content: "已改好标题。" };
  assert.equal(lastAssistantInCurrentTurn(prior), "");
  assert.equal(lastAssistantContent(prior), "依赖已经列出来了。");
  assert.equal(coversLive(prior, live), false);
  assert.deepEqual(preferFresherHistory(prior, prior), prior);
}

assert.equal(
  stripLeakedToolMarkup(
    "先确认上传文件是否存在。\n\n<tool_calls>\n</tool_calls>\n\n<tool_result>\n</tool_result>\n\n文件存在，读取内容。",
  ),
  "先确认上传文件是否存在。",
);
assert.equal(stripLeakedToolMarkup("正常回复，没有工具标记。"), "正常回复，没有工具标记。");

{
  const cited =
    "P5 前端 `stripLeakedToolMarkup` 缺围栏豁免 → 我提 `<tool_result>` 时被截断。\n- P6 空壳一律判 parse_fail。\n- P7 `probe_client` 没跟上。";
  assert.equal(stripLeakedToolMarkup(cited), cited);
  assert.equal(
    stripLeakedToolMarkup("分析里写 `<tool_call>` 是引用。\n\n<tool_calls>\n</tool_calls>\n后面是泄漏。"),
    "分析里写 `<tool_call>` 是引用。",
  );
  assert.equal(
    stripLeakedToolMarkup("围栏里的不算：\n```\n<tool_result>\n</tool_result>\n```\n正文继续。"),
    "围栏里的不算：\n```\n<tool_result>\n</tool_result>\n```\n正文继续。",
  );
}

assert.equal(
  stripThinkRestatement("把登录页标题改成 ixiaotao", "用户想把登录页标题改成 ixiaotao。\n先改 auth.tsx。"),
  "先改 auth.tsx。",
);
assert.equal(
  stripThinkRestatement("fix the login title", "The user wants me to fix the login title. I'll open auth.tsx."),
  "I'll open auth.tsx.",
);
assert.equal(
  stripThinkRestatement("修 paging", "auth.rs 里 page_bounds 的起算是 1-based。"),
  "auth.rs 里 page_bounds 的起算是 1-based。",
);

assert.equal(isPrepareHint("正在连接模型…\n"), true);
assert.equal(isPrepareHint("正在准备工作区…"), true);
assert.equal(isPrepareHint("hmm, the user asked…"), false);
