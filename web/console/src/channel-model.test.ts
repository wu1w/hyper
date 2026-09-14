import assert from "node:assert/strict";
import {
  addableAction,
  addableKinds,
  cardStatus,
  channelEnableHint,
  extraRecord,
  isBound,
  matchLine,
  mergeQrCreds,
  newChannelRow,
  nextEpId,
  policyName,
  qrPollGap,
  qrStatusLabel,
  runtimePill,
  toChannelPayload,
  upsertBody,
  parseTagDraft,
  mergeTags,
  qrFailMessage,
  qrPollDone,
  channelCardClass,
  qrButtonLabel,
  addChannelPlan,
  extraStr,
} from "./channel-model.ts";
import type { ChannelEp, ChannelKind } from "./api.ts";

const qq: ChannelKind = {
  id: "qq",
  name: "QQ",
  blurb: "QQ",
  mark: "Q",
  color: "#00",
  qr: true,
  once: true,
  in_process: true,
  fields: [{ key: "app_id", label: "App", secret: true }],
};
const webhook: ChannelKind = {
  id: "webhook",
  name: "Webhook",
  blurb: "HTTP",
  mark: "W",
  color: "#00",
  qr: false,
  once: false,
  in_process: true,
  fields: [],
};

assert.equal(policyName("dm"), "白名单");
assert.equal(policyName("group"), "需提及");
assert.equal(policyName("group", "open"), "开放");
assert.equal(policyName("group", "mention"), "需提及");
assert.equal(qrPollGap("feishu"), 5000);
assert.equal(qrPollGap("qq"), 3000);
assert.equal(qrPollGap("telegram"), 1500);
assert.equal(qrStatusLabel("waiting", "qq"), "等待扫码 · QQ 开通机器人可能要一会儿");
assert.equal(qrStatusLabel("success", "qq", true), "凭证已写入，正在连接");
assert.equal(qrStatusLabel("expired"), "二维码已过期");
assert.ok(channelEnableHint(qq).includes("QQ"));
assert.ok(channelEnableHint({ ...webhook, in_process: false }).includes("尚未进进程"));

const ep: ChannelEp = {
  id: "qq",
  kind: "qq",
  enabled: true,
  require_mention: true,
  dm_policy: "allowlist",
  group_policy: "mention",
  extra: { app_id: "1", skip: 2 as unknown as string },
};
assert.deepEqual(extraRecord(ep.extra), { app_id: "1" });
assert.ok(matchLine({ ...ep, allow_from: ["a", "b"], group_policy: "open" }).includes("群需@"));
assert.ok(matchLine({ ...ep, allow_from: ["a", "b"], group_policy: "open" }).includes("白名单 2"));
assert.ok(!matchLine({ ...ep, require_mention: false, group_policy: "open" }).includes("群需@"));
assert.equal(extraStr({ ...ep, extra: { skip: 2 as unknown as string } }, "skip"), "");
assert.equal(extraStr(ep, "app_id"), "1");
assert.equal(nextEpId([], "qq"), "qq");
assert.equal(nextEpId([ep], "qq"), "qq-2");

const payload = toChannelPayload([{ ...ep, bot_token: " tok ", kind: "telegram" }])[0];
assert.equal(payload.extra.bot_token, "tok");

assert.equal(isBound({ ...ep, creds_set: ["app_id"] }, qq), true);
assert.equal(isBound({ ...ep, creds_set: [] }, qq), false);
assert.equal(runtimePill({ ...ep, runtime: { state: "retry" } })?.label, "重试中");
assert.equal(runtimePill({ ...ep, runtime: { state: "running" } })?.label, "运行中");
assert.equal(runtimePill({ ...ep, runtime: { state: "error" } })?.label, "连接错误");
assert.equal(runtimePill({ ...ep, runtime: { state: "no_credentials" } })?.label, "缺凭证");
assert.equal(runtimePill({ ...ep, runtime: { state: "idle" } })?.label, "未连接");
assert.equal(cardStatus({ ...ep, enabled: false }).label, "未启用");

const catalog = [qq, webhook];
assert.deepEqual(
  addableKinds(catalog, [{ ...ep }]).map((c) => c.id),
  ["webhook"],
);
const row = newChannelRow("webhook", []);
assert.equal(row.id, "webhook");
assert.equal(row.bind, "127.0.0.1:8788");
assert.equal(row.dm_policy, "open");
const feishu = newChannelRow("feishu", []);
assert.equal(feishu.extra?.domain, "feishu");
assert.equal(feishu.require_mention, true);

const bound = mergeQrCreds(ep, { app_id: "x", empty: "" });
assert.equal(bound.extra?.app_id, "x");
assert.equal(bound.enabled, true);
assert.equal(bound._local, false);

const body = upsertBody({ ...ep, _origId: "old", id: "new" });
assert.equal(body.rename, "old");
assert.equal(body.upsert.id, "new");
assert.equal(addableAction(webhook, true), "再添加");
assert.equal(addableAction(qq, false), "扫码");
assert.deepEqual(parseTagDraft("a, b，c"), ["a", "b", "c"]);
assert.deepEqual(mergeTags(["a"], ["a", "b"]), ["a", "b"]);
assert.equal(qrFailMessage("expired"), "二维码已过期，请重新获取");
assert.equal(qrFailMessage("fail", "nope"), "nope");
assert.equal(qrFailMessage("fail"), "授权失败");
assert.equal(qrPollDone("success"), true);
assert.equal(qrPollDone("fail"), true);
assert.equal(qrPollDone("expired"), true);
assert.equal(qrPollDone("waiting"), false);
assert.equal(channelCardClass(true, false), "card ch-card on dim");
assert.equal(channelCardClass(false, true), "card ch-card");
assert.equal(qrButtonLabel(true), "刷新二维码");
assert.equal(qrButtonLabel(false), "获取二维码");
assert.deepEqual(addChannelPlan("qq", catalog, [{ ...ep }]), { action: "open", index: 0 });
assert.deepEqual(addChannelPlan("webhook", catalog, []), { action: "create" });
assert.deepEqual(addChannelPlan("webhook", catalog, [{ ...ep, kind: "webhook", id: "webhook" }]), {
  action: "create",
});
assert.deepEqual(addChannelPlan("qq", [{ ...qq, in_process: false }], []), { action: "ignore" });
