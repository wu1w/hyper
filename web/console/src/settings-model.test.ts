import assert from "node:assert/strict";
import {
  authCards,
  authModeWire,
  chatOrigin,
  inferAuthMode,
  loginLabel,
  normalizeAuthMode,
  parseEnvIgnored,
  parseSteps,
  parseTok,
  probeStatusText,
  sessionLabel,
  settingsFormError,
  SESSION_URL,
  windowPresetId,
  wireExtra,
  XAI_URL,
  familyLine,
  pickAuthUrl,
  imageEndpointHint,
  probeDotVar,
  oauthWaiting,
  showOauthError,
  showOauthLink,
  showDeviceOverlay,
  deviceOpenUrl,
  persistServerBody,
  persistApplyBody,
} from "./settings-model.ts";

assert.equal(parseTok("8k"), 8192);
assert.equal(parseTok("500k"), 500000);
assert.equal(parseTok("262k"), 262144);
assert.equal(parseTok("262144"), 262144);
assert.equal(parseTok("1m"), 1024 * 1024);
assert.equal(parseTok("0"), null);
assert.equal(parseTok("nope"), null);
assert.equal(parseTok(" 32K "), 32768);

assert.equal(parseSteps("500"), 500);
assert.equal(parseSteps("0"), null);
assert.equal(parseSteps("1"), 1);
assert.equal(parseSteps("10000"), 10000);
assert.equal(parseSteps("10001"), null);
assert.equal(parseSteps("10_000"), null);

assert.equal(inferAuthMode(SESSION_URL), "session");
assert.equal(inferAuthMode(XAI_URL), "api_key");
assert.equal(inferAuthMode("http://127.0.0.1:8080/v1"), "custom");
assert.equal(normalizeAuthMode("openai_compat", SESSION_URL), "custom");
assert.equal(normalizeAuthMode(undefined, SESSION_URL), "session");
assert.equal(chatOrigin("session", "https://example.com"), SESSION_URL);
assert.equal(chatOrigin("custom", "https://example.com/v1/"), "https://example.com/v1");
assert.equal(authModeWire("custom"), "openai_compat");
assert.equal(authModeWire("session"), "session");

assert.equal(loginLabel(true), "已登录");
assert.equal(loginLabel(false), "未登录");
assert.equal(sessionLabel("valid", false), "已登录");
assert.equal(sessionLabel("expired", true), "已过期");
assert.equal(sessionLabel("absent", true), "未登录");

assert.deepEqual(parseEnvIgnored(["HYPER_FOO"]), ["HYPER_FOO"]);
assert.deepEqual(parseEnvIgnored({ MODEL: true, HYPER_BASE_URL: false }), ["HYPER_MODEL"]);
assert.equal(windowPresetId(8192), "8192");
assert.equal(windowPresetId(123), "custom");
assert.equal(windowPresetId(null), "custom");

assert.equal(
  settingsFormError({ authMode: "custom", url: "not-a-url", imageUrl: "" }),
  "自定义端点需以 http:// 或 https:// 开头",
);
assert.equal(
  settingsFormError({ authMode: "session", url: "", imageUrl: "ftp://x" }),
  "生图端点需以 http:// 或 https:// 开头，或留空沿用对话端点",
);
assert.equal(
  settingsFormError({
    authMode: "session",
    url: "",
    imageUrl: "",
    windowTok: "nope",
    maxSteps: "10",
    requireWindow: true,
  }),
  "working_window 无效",
);
assert.equal(
  settingsFormError({
    authMode: "api_key",
    url: "",
    imageUrl: "",
    windowTok: "128k",
    maxSteps: "0",
    requireWindow: true,
  }),
  "max_steps 须为 1–10000 的整数",
);
assert.equal(
  settingsFormError({
    authMode: "session",
    url: "",
    imageUrl: "",
    windowTok: "500k",
    maxSteps: "500",
    requireWindow: true,
  }),
  null,
);

assert.equal(probeStatusText({ busy: true, ok: null }, ""), "正在探测端点…");
assert.equal(probeStatusText({ busy: false, ok: true, model: "grok-4.6" }, "responses"), "模型可达 · grok-4.6 · Responses");
assert.equal(probeStatusText({ busy: false, ok: false }, ""), "模型不可达");
assert.equal(authCards({ sessionState: "valid", loggedIn: true, keySet: true, wire: "responses" })[0].extra, "会话 已登录");
assert.equal(familyLine(""), "family 检测中 / 未接入");
assert.ok(familyLine("grok46-x").includes("grok46"));
assert.equal(familyLine("gpt"), "family gpt");
assert.equal(pickAuthUrl("session"), SESSION_URL);
assert.equal(pickAuthUrl("api_key"), XAI_URL);
assert.equal(pickAuthUrl("custom"), undefined);
assert.ok(imageEndpointHint("session").includes("OAuth"));
assert.ok(imageEndpointHint("api_key").includes("api.x.ai"));
assert.ok(imageEndpointHint("custom").includes("对话 base_url"));
assert.equal(probeDotVar(true), "var(--ok)");
assert.equal(probeDotVar(false), "var(--danger)");
assert.equal(probeDotVar(null), "var(--label-3)");
assert.equal(oauthWaiting({ phase: "waiting" }), true);
assert.equal(oauthWaiting({ phase: "ok" }), false);
assert.equal(showOauthError("session", { phase: "error", error: "x" }), true);
assert.equal(showOauthError("api_key", { phase: "error", error: "x" }), false);
assert.equal(showOauthError("session", { phase: "error" }), false);
assert.equal(showOauthLink("session", { phase: "waiting", kind: "oauth", authorize_url: "https://x" }), true);
assert.equal(showOauthLink("session", { phase: "waiting", kind: "oauth" }), false);
assert.equal(showOauthLink("api_key", { phase: "waiting", kind: "oauth", authorize_url: "https://x" }), false);
assert.equal(showDeviceOverlay({ phase: "waiting", kind: "device" }), true);
assert.equal(showDeviceOverlay({ phase: "waiting", kind: "oauth" }), false);
assert.equal(deviceOpenUrl({ verification_uri: "https://a", verification_uri_complete: "https://b" }), "https://b");
assert.equal(deviceOpenUrl({ verification_uri: "https://a" }), "https://a");
assert.deepEqual(
  persistServerBody({
    authMode: "session",
    url: "ignored",
    key: "secret",
    model: "g",
    imageUrl: " https://img ",
    imageModel: " imagine ",
  }),
  {
    auth_mode: "session",
    base_url: SESSION_URL,
    api_key: undefined,
    model: "g",
    image_base_url: "https://img",
    image_model: "imagine",
  },
);
assert.equal(
  persistApplyBody({
    authMode: "api_key",
    url: "",
    key: "k",
    model: "g",
    imageUrl: "",
    imageModel: "",
    lossy: true,
    windowTok: "8k",
    maxSteps: "500",
  }).working_window,
  8192,
);
assert.equal(
  persistApplyBody({
    authMode: "api_key",
    url: "",
    key: "k",
    model: "g",
    imageUrl: "",
    imageModel: "",
    lossy: true,
    windowTok: "8k",
    maxSteps: "500",
  }).max_steps,
  500,
);
const apply = persistApplyBody({
  authMode: "api_key",
  url: "",
  key: "k",
  model: "g",
  imageUrl: "",
  imageModel: "",
  lossy: true,
  windowTok: "8k",
  maxSteps: "500",
});
assert.equal(apply.low_precision, true);
assert.equal(apply.api_key, "k");
