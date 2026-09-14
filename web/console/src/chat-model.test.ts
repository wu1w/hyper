import assert from "node:assert/strict";
import {
  agentStatusLabel,
  attachPayload,
  busyPolicyHint,
  cacheHitLabel,
  clipEnd,
  composerHintLine,
  composerKeyPlan,
  composerPlaceholder,
  detailsRailClass,
  detailsRailStyle,
  editorPayload,
  firstLine,
  fmtTokS,
  fuzzyFiles,
  fuzzyScore,
  hiddenNote,
  imeBusy,
  insertAtCaret,
  isAskTool,
  isHarnessNote,
  isHostImageTool,
  isTaskTool,
  isTodoTool,
  mentionToken,
  parseJsonObj,
  parseTaskOutput,
  pickAgentStatus,
  previewPathsOf,
  quietStopReason,
  sendButtonLabel,
  stepStatusOf,
  thinkTail,
  toolBadge,
  toolKey,
  toolLabel,
  waitPrefixBit,
  windowGauge,
} from "./chat-model.ts";

assert.equal(imeBusy({ isComposing: true, nativeEvent: {}, keyCode: 13 }), true);
assert.equal(imeBusy({ nativeEvent: { isComposing: true }, keyCode: 13 }), true);
assert.equal(imeBusy({ nativeEvent: {}, keyCode: 229 }), true);
assert.equal(imeBusy({ nativeEvent: {}, keyCode: 13 }), false);

assert.equal(fuzzyScore("", "abc"), 0);
assert.equal(fuzzyScore("ab", "ab"), 2000);
assert.ok((fuzzyScore("ab", "xxabc") as number) > (fuzzyScore("ab", "zzzzab") as number));
assert.equal(fuzzyScore("xyz", "ab"), null);
assert.ok(fuzzyScore("abc", "aXbXc") != null);

const files = [
  { path: "src/app.ts", name: "app.ts", dir: false },
  { path: "src/node_modules/x", name: "x", dir: false },
  { path: "src/lib", name: "lib", dir: true },
];
assert.equal(fuzzyFiles(files, "app")[0]?.name, "app.ts");
assert.ok(!fuzzyFiles(files, "").some((f) => f.path.includes("node_modules")));
assert.equal(fuzzyFiles(files, "").length, 2);

assert.equal(mentionToken("/help @x", 8), null);
assert.deepEqual(mentionToken("see @fo", 7), { start: 4, end: 7, query: "fo" });
assert.equal(mentionToken("see @fo bar", 9), null);
assert.equal(mentionToken("a@x", 3), null);

assert.deepEqual(insertAtCaret("ab", "X", 1, 1), { next: "aXb", caret: 2 });
assert.deepEqual(editorPayload("/tmp/a.ts"), { active: "tmp/a.ts", files: [{ path: "tmp/a.ts" }] });
assert.deepEqual(editorPayload("  "), { files: [] });

assert.equal(
  attachPayload("hi", [
    { name: "a.txt", path: "a.txt", url: "", mime: "text/plain", kind: "file", content_part: {} },
  ]).prompt.includes("[attached: a.txt]"),
  true,
);
assert.equal(attachPayload("", []).prompt, " ");
assert.equal(
  attachPayload("hi", [
    {
      name: "a.png",
      path: "a.png",
      url: "",
      mime: "image/png",
      kind: "image",
      content_part: { type: "image" },
    },
  ]).content_parts.length,
  1,
);

assert.equal(hiddenNote("plain"), null);
assert.equal(hiddenNote("<tool_response>[trajectory] x</tool_response>"), "");
assert.equal(hiddenNote("<tool_response>hello</tool_response>"), "hello");
assert.equal(isHarnessNote("HYPER_WORKING_WINDOW=1"), true);
assert.equal(isHarnessNote("MEMORY.md"), true);
assert.equal(isHarnessNote("hello"), false);

assert.equal(firstLine("\n  hi\n"), "hi");
assert.equal(firstLine("\n\n"), "");
assert.equal(clipEnd("abcd", 10), "abcd");
assert.equal(clipEnd("abcdef", 4), "abc…");
assert.ok(thinkTail("a\n" + "x".repeat(80)).startsWith("…"));
assert.equal(thinkTail("x".repeat(20)), "x".repeat(20));

assert.equal(pickAgentStatus("running"), "running");
assert.equal(pickAgentStatus(null, "failed"), "failed");
assert.equal(pickAgentStatus("queued"), "queued");
assert.equal(pickAgentStatus(null, "done"), "done");
assert.equal(pickAgentStatus(null, undefined, 0), "done");

assert.equal(quietStopReason("stop"), true);
assert.equal(quietStopReason("parse failed"), true);
assert.equal(quietStopReason("budget:tokens"), true);
assert.equal(quietStopReason("Max iterations"), true);
assert.equal(quietStopReason("boom"), false);
assert.equal(stepStatusOf(false), "run");
assert.equal(stepStatusOf(true, "tool task aborted"), "warn");
assert.equal(stepStatusOf(true, "Error: x"), "err");
assert.equal(stepStatusOf(true, "ok"), "ok");

assert.equal(fmtTokS(120), "120 tok/s");
assert.equal(fmtTokS(100), "100 tok/s");
assert.equal(fmtTokS(12.34), "12.3 tok/s");
assert.equal(fmtTokS(1.234), "1.23 tok/s");
assert.equal(toolKey("Web_Search"), "websearch");
assert.equal(toolBadge("Read"), "read");
assert.equal(toolBadge("StrReplace"), "edit");
assert.equal(toolBadge("Write"), "write");
assert.equal(toolBadge("Shell"), "bash");
assert.equal(isTodoTool("TodoWrite"), true);
assert.equal(isTodoTool("todo"), true);
assert.equal(isTaskTool("Task"), true);
assert.equal(isTaskTool("SpawnSubagent"), true);
assert.deepEqual(parseTaskOutput("BACKGROUND abc\n"), { id: "abc", status: "running" });
assert.deepEqual(parseTaskOutput("STATUS done id=z"), { status: "done", id: "z" });
assert.deepEqual(parseTaskOutput("plain"), {});
assert.equal(agentStatusLabel("running"), "运行中");
assert.equal(agentStatusLabel("completed"), "已完成");
assert.equal(agentStatusLabel("canceled"), "已取消");
assert.equal(isAskTool("AskQuestion"), true);
assert.equal(isHostImageTool("ImageGeneration"), true);
assert.equal(isHostImageTool("Read"), false);
assert.equal(toolLabel("ImageGeneration"), "生成图片");
assert.equal(toolLabel("Read"), "Read");
assert.deepEqual(parseJsonObj('{"a":1}'), { a: 1 });
assert.deepEqual(parseJsonObj(""), {});
assert.equal(parseJsonObj("[1]"), null);
assert.equal(parseJsonObj("nope"), null);

assert.equal(sendButtonLabel(false, "steer", false), "发送");
assert.equal(sendButtonLabel(false, "steer", true), "生成");
assert.equal(sendButtonLabel(true, "queue", false), "排队");
assert.equal(sendButtonLabel(true, "steer", false), "转向");
assert.equal(sendButtonLabel(true, "interrupt", false), "打断");
assert.ok(composerPlaceholder(false, "steer", true).includes("生图端点"));
assert.ok(composerPlaceholder(false, "steer", false).includes("发消息"));
assert.ok(composerPlaceholder(true, "queue", false).includes("本轮结束"));
assert.ok(busyPolicyHint("queue").includes("queue"));
assert.ok(busyPolicyHint("steer").includes("steer"));
assert.ok(busyPolicyHint("interrupt").includes("interrupt"));
assert.ok(composerHintLine(true, "steer", false).includes("转向 / 排队"));
assert.ok(composerHintLine(false, "steer", true).includes("生成图片"));
assert.ok(composerHintLine(false, "steer", false).includes("Enter 发送"));

assert.equal(composerKeyPlan({ composing: true, key: "Enter", shiftKey: false, slashN: 0, mentionN: 0, typed: "x" }).act, "skip");
assert.equal(composerKeyPlan({ composing: false, key: "Escape", shiftKey: false, slashN: 1, mentionN: 0, typed: "/" }).act, "clearSlash");
assert.equal(composerKeyPlan({ composing: false, key: "Escape", shiftKey: false, slashN: 0, mentionN: 1, typed: "@" }).act, "clearMentions");
assert.deepEqual(composerKeyPlan({ composing: false, key: "ArrowDown", shiftKey: false, slashN: 2, mentionN: 0, typed: "/" }), {
  act: "navSlash",
  dir: 1,
});
assert.deepEqual(composerKeyPlan({ composing: false, key: "ArrowUp", shiftKey: false, slashN: 0, mentionN: 2, typed: "@" }), {
  act: "navMentions",
  dir: -1,
});
assert.equal(composerKeyPlan({ composing: false, key: "Tab", shiftKey: false, slashN: 1, mentionN: 0, typed: "/" }).act, "applySlash");
assert.equal(composerKeyPlan({ composing: false, key: "Tab", shiftKey: false, slashN: 0, mentionN: 1, typed: "@" }).act, "applyMention");
assert.deepEqual(
  composerKeyPlan({ composing: false, key: "Enter", shiftKey: false, slashN: 1, mentionN: 0, typed: "/plan", slashCmd: "/plan" }),
  { act: "enterSlash", full: true },
);
assert.deepEqual(
  composerKeyPlan({ composing: false, key: "Enter", shiftKey: false, slashN: 1, mentionN: 0, typed: "/plan go", slashCmd: "/plan" }),
  { act: "enterSlash", full: true },
);
assert.deepEqual(
  composerKeyPlan({ composing: false, key: "Enter", shiftKey: false, slashN: 1, mentionN: 0, typed: "/p", slashCmd: "/plan" }),
  { act: "enterSlash", full: false },
);
assert.equal(composerKeyPlan({ composing: false, key: "Enter", shiftKey: true, slashN: 0, mentionN: 0, typed: "hi" }).act, "skip");
assert.equal(composerKeyPlan({ composing: false, key: "Enter", shiftKey: false, slashN: 0, mentionN: 1, typed: "@" }).act, "enterMention");
assert.equal(composerKeyPlan({ composing: false, key: "Enter", shiftKey: false, slashN: 0, mentionN: 0, typed: "hi" }).act, "enterSend");

assert.equal(detailsRailClass({ open: false, tab: "session", previewPath: "", previewMax: false }), "details closed");
assert.ok(detailsRailClass({ open: true, tab: "preview", previewPath: "a.ts", previewMax: true }).includes("wide"));
assert.ok(detailsRailClass({ open: true, tab: "agent", previewPath: "", previewMax: false }).includes("agent-open"));
assert.deepEqual(detailsRailStyle(true, "preview", "a.ts", false, 400), { width: 400, flex: "0 0 auto" });
assert.equal(detailsRailStyle(true, "preview", "a.ts", true, 400), undefined);
assert.deepEqual(previewPathsOf(["a"], ["b"], ["c"]), ["a"]);
assert.deepEqual(previewPathsOf([], ["b"], ["c"]), ["b"]);
assert.deepEqual(previewPathsOf([], [], ["c"]), ["c"]);
assert.deepEqual(windowGauge(50, 100), { rawPct: 50, pct: 50 });
assert.deepEqual(windowGauge(0, 0), { rawPct: 0, pct: 0 });
assert.equal(windowGauge(250, 100).pct, 100);
assert.equal(cacheHitLabel(true, 12.34), "12.3%");
assert.equal(cacheHitLabel(false, 12.34), "n/a");
assert.equal(waitPrefixBit(12), " · 12 tokens");
assert.equal(waitPrefixBit(0), "");
