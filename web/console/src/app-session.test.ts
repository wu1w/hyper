import assert from "node:assert/strict";
import type { Clarify, Permit, SessionEvent, Snap } from "./api.ts";
import {
  applyDelta,
  beginTurnLive,
  emptyLive,
  failTurnLive,
  handleConsoleRpc,
  handleEventAppend,
  handleHello,
  handleHistoryReplace,
  handleResync,
  ignoreHistoryReplace,
  lastAssistantDup,
  modalForFocus,
  parkReload,
  type RpcCtx,
  type Transcript,
} from "./app-session.ts";
import { isPrepareHint, PREPARE_HINT } from "./chat-live.ts";

assert.deepEqual(applyDelta(emptyLive(), { type: "delta", text: "hi" }), { think: "", content: "hi" });
assert.deepEqual(applyDelta(emptyLive(), { type: "delta", channel: "reasoning", text: "t" }), {
  think: "t",
  content: "",
});
assert.deepEqual(applyDelta({ think: "a", content: "b" }, { type: "delta", reset: true }), emptyLive());
assert.deepEqual(
  applyDelta({ think: "a", content: "b" }, { type: "delta", reset: true, content_only: true }),
  { think: "a", content: "" },
);

const permit: Permit = { id: 1, tool: "Write", preview: "x", session: "s1" };
assert.deepEqual(modalForFocus(permit, "s1"), permit);
assert.equal(modalForFocus(permit, "s2"), null);
assert.deepEqual(modalForFocus(permit, undefined), permit);
assert.equal(modalForFocus(null, "s1"), null);

const asst: SessionEvent = { type: "assistant", content: "好了" };
assert.equal(lastAssistantDup([asst], { type: "assistant", content: "好了" }), true);
assert.equal(lastAssistantDup([asst], { type: "assistant", content: "别的" }), false);
assert.equal(lastAssistantDup([], { type: "assistant", content: "" }), false);
assert.equal(ignoreHistoryReplace({ session: "other" }, "s1"), true);
assert.equal(ignoreHistoryReplace({ session: "other", events: [] }, "s1"), false);

assert.deepEqual(beginTurnLive(emptyLive(), PREPARE_HINT), { think: PREPARE_HINT, content: "" });
assert.deepEqual(beginTurnLive({ think: "x", content: "" }, PREPARE_HINT), { think: "x", content: "" });
assert.deepEqual(failTurnLive({ think: PREPARE_HINT, content: "" }, isPrepareHint), emptyLive());
assert.deepEqual(failTurnLive({ think: "real", content: "" }, isPrepareHint), { think: "real", content: "" });

{
  const parked: Transcript = { events: [{ type: "user", text: "a" }], live: { think: "t", content: "" } };
  const incoming: SessionEvent[] = [{ type: "user", text: "a" }, asst];
  const same = parkReload("s1", { session: "s1" }, incoming, parked);
  assert.equal(same.switched, false);
  const switched = parkReload("old", { session: "s2" }, incoming, undefined);
  assert.equal(switched.switched, true);
}

function mockCtx(over: Partial<RpcCtx> = {}): RpcCtx & { snaps: Snap[]; lives: unknown[]; permits: Permit[] } {
  const snaps: Snap[] = [];
  const lives: unknown[] = [];
  const permits: Permit[] = [];
  const ctx: RpcCtx & { snaps: Snap[]; lives: unknown[]; permits: Permit[] } = {
    snaps,
    lives,
    permits,
    session: () => "s1",
    transcripts: {},
    setSnap: (s) => snaps.push(s),
    setEvents: () => {},
    setLive: (l) => lives.push(typeof l === "function" ? l(emptyLive()) : l),
    setPermit: (p) => permits.push(p),
    setClarify: () => {},
    setPendingTurn: () => {},
    scheduleLive: () => {},
    pullHistory: () => {},
    cancelLiveRaf: () => {},
    ...over,
  };
  return ctx;
}

{
  const ctx = mockCtx();
  handleHello({ state: { session: "s1" }, events: [{ type: "user", text: "hi" }], permit }, ctx);
  assert.equal(ctx.snaps[0]?.session, "s1");
  assert.equal(ctx.permits[0]?.id, 1);
  assert.deepEqual(ctx.transcripts.s1.live, emptyLive());
}

{
  const ctx = mockCtx();
  handleConsoleRpc({ method: "permit.ask", params: { ...permit, session: "s2" } }, ctx);
  assert.equal(ctx.permits.length, 0);
  handleConsoleRpc({ method: "permit.ask", params: permit }, ctx);
  assert.equal(ctx.permits[0]?.id, 1);
  handleConsoleRpc({ method: "permit.clear", params: {} }, ctx);
  assert.equal(ctx.permits[1], null);
}

{
  const ctx = mockCtx();
  handleEventAppend({ type: "delta", session: "s1", text: "ab" }, ctx);
  assert.equal(ctx.transcripts.s1.live.content, "ab");
  handleEventAppend({ type: "assistant", session: "s1", content: "好了" }, ctx);
  assert.equal(ctx.transcripts.s1.events.length, 1);
  handleEventAppend({ type: "assistant", session: "s1", content: "好了" }, ctx);
  assert.equal(ctx.transcripts.s1.events.length, 1);
  handleEventAppend({ type: "stop", session: "s1" }, ctx);
  assert.equal(ctx.transcripts.s1.live.think, "");
}

{
  const ctx = mockCtx();
  handleHistoryReplace({ reset: true, session: "s1", events: [{ type: "user", text: "n" }] }, ctx);
  assert.deepEqual(ctx.transcripts.s1.events, [{ type: "user", text: "n" }]);
  assert.deepEqual(ctx.transcripts.s1.live, emptyLive());
}

{
  const clarify: Clarify = { id: 2, title: "?", prompt: "x", options: [], session: "s1" };
  const ctx = mockCtx();
  handleConsoleRpc({ method: "clarify.ask", params: clarify }, ctx);
  handleConsoleRpc({ method: "state", params: { session: "s1", model: "g" } }, ctx);
  assert.equal(ctx.snaps[0]?.model, "g");
}

{
  const ctx = mockCtx();
  handleResync({ state: { session: "s1" }, events: [{ type: "user", text: "r" }] }, ctx);
  assert.equal(ctx.snaps[0]?.session, "s1");
  assert.deepEqual(ctx.transcripts.s1.events, [{ type: "user", text: "r" }]);
  let pulls = 0;
  ctx.pullHistory = () => {
    pulls += 1;
  };
  handleResync({ state: { session: "s1" } }, ctx);
  assert.equal(pulls, 1);
}
