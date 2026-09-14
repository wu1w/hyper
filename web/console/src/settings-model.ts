export const WINDOW_PRESETS: Array<{ id: string; n: number; label: string }> = [
  { id: "8192", n: 8192, label: "8k" },
  { id: "32768", n: 32768, label: "32k" },
  { id: "131072", n: 131072, label: "128k" },
  { id: "262144", n: 262144, label: "256k" },
  { id: "500000", n: 500000, label: "500k" },
];

export const IMAGE_MODELS = [
  "grok-imagine-image-2.0",
  "grok-imagine-image",
  "grok-2-image-1212",
  "grok-2-image",
];

export const PRICE_CLIFF = 200_000;
export const SESSION_URL = "https://cli-chat-proxy.grok.com/v1";
export const XAI_URL = "https://api.x.ai/v1";

export type AuthMode = "session" | "api_key" | "custom";

export function parseTok(s: string): number | null {
  const t = s.trim().toLowerCase().replace(/,/g, "").replace(/_/g, "");
  const preset = WINDOW_PRESETS.find((p) => p.id === t || p.label.toLowerCase() === t);
  if (preset) return preset.n;
  // Old label; 262144 = 256 × 1024, not 262 × 1024.
  if (t === "262k") return 262144;
  const m = t.match(/^(\d+(?:\.\d+)?)([km])?$/);
  if (!m) return null;
  let n = Number(m[1]);
  if (!Number.isFinite(n)) return null;
  if (m[2] === "k") n *= 1024;
  if (m[2] === "m") n *= 1024 * 1024;
  n = Math.round(n);
  return n > 0 ? n : null;
}

export function parseSteps(s: string): number | null {
  const t = s.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  if (!Number.isInteger(n) || n < 1 || n > 10_000) return null;
  return n;
}

export function inferAuthMode(url: string): AuthMode {
  const u = (url || "").toLowerCase();
  if (u.includes("cli-chat-proxy")) return "session";
  if (u.includes("api.x.ai")) return "api_key";
  return "custom";
}

export function normalizeAuthMode(mode: string | undefined, url: string): AuthMode {
  if (mode === "session" || mode === "api_key") return mode;
  if (mode === "custom" || mode === "openai_compat") return "custom";
  return inferAuthMode(url);
}

export function chatOrigin(mode: AuthMode, customUrl: string): string {
  if (mode === "session") return SESSION_URL;
  if (mode === "api_key") return XAI_URL;
  return customUrl.trim().replace(/\/+$/, "");
}

export function authModeWire(mode: AuthMode): string {
  return mode === "custom" ? "openai_compat" : mode;
}

export function loginLabel(v: boolean | null): string {
  if (v === true) return "已登录";
  if (v === false) return "未登录";
  return "检测中 / 未接入";
}

export function sessionLabel(state: string, loggedIn: boolean | null): string {
  if (state === "valid") return "已登录";
  if (state === "expired") return "已过期";
  if (state === "absent") return "未登录";
  return loginLabel(loggedIn);
}

export function parseEnvIgnored(raw: Record<string, boolean> | string[] | undefined): string[] {
  if (Array.isArray(raw)) return raw;
  return Object.entries(raw || {})
    .filter(([, v]) => v)
    .map(([k]) => (k.startsWith("HYPER_") ? k : `HYPER_${k.toUpperCase()}`));
}

export function windowPresetId(parsed: number | null): string {
  return parsed && WINDOW_PRESETS.some((p) => p.n === parsed) ? String(parsed) : "custom";
}

export function settingsFormError(input: {
  authMode: AuthMode;
  url: string;
  imageUrl: string;
  windowTok?: string;
  maxSteps?: string;
  requireWindow?: boolean;
}): string | null {
  const base = chatOrigin(input.authMode, input.url);
  if (!/^https?:\/\//.test(base)) return "自定义端点需以 http:// 或 https:// 开头";
  const image = input.imageUrl.trim();
  if (image && !/^https?:\/\//.test(image)) return "生图端点需以 http:// 或 https:// 开头，或留空沿用对话端点";
  if (input.requireWindow) {
    if (parseTok(input.windowTok || "") == null) return "working_window 无效";
    if (parseSteps(input.maxSteps || "") == null) return "max_steps 须为 1–10000 的整数";
  }
  return null;
}

export function probeStatusText(
  probe: { busy: boolean; ok: boolean | null; model?: string },
  wire: string,
): string {
  if (probe.busy) return "正在探测端点…";
  if (probe.ok === true) {
    const wireBit =
      wire === "responses"
        ? "Responses"
        : wire === "chat_completions"
          ? "Chat Completions"
          : wire;
    return `模型可达${probe.model ? ` · ${probe.model}` : ""}${wire ? ` · ${wireBit}` : ""}`;
  }
  if (probe.ok === false) return "模型不可达";
  return "未探测";
}

export function wireExtra(wire: string): string {
  if (wire === "responses") return "Responses · Cursor 线";
  if (wire === "chat_completions") return "Chat Completions";
  return "自动适配线协议";
}

export type OauthState = {
  phase?: string;
  kind?: string;
  authorize_url?: string;
  user_code?: string;
  verification_uri?: string;
  verification_uri_complete?: string;
  error?: string;
};

export function pickAuthUrl(mode: AuthMode): string | undefined {
  if (mode === "session") return SESSION_URL;
  if (mode === "api_key") return XAI_URL;
  return undefined;
}

export function imageEndpointHint(mode: AuthMode): string {
  if (mode === "session") {
    return "Imagine REST（POST /v1/images/generations）。留空则走 grok login 会话端点，同一套 OAuth 凭据。";
  }
  if (mode === "api_key") {
    return "Imagine REST（POST /v1/images/generations）。留空则走 https://api.x.ai/v1，同一套 API key。";
  }
  return "Imagine REST（POST /v1/images/generations）。留空则用上面的对话 base_url。";
}

export function familyLine(family: string): string {
  const grok46 = family.toLowerCase().includes("grok46") ? " · grok46" : "";
  return `family ${family ? family : "检测中 / 未接入"}${grok46}`;
}

export function probeDotVar(ok: boolean | null): string {
  if (ok === true) return "var(--ok)";
  if (ok === false) return "var(--danger)";
  return "var(--label-3)";
}

export function oauthWaiting(oauth?: OauthState | null): boolean {
  return oauth?.phase === "waiting";
}

export function showOauthError(authMode: AuthMode, oauth?: OauthState | null): boolean {
  return authMode === "session" && !!oauth?.error && oauth.phase === "error";
}

export function showOauthLink(authMode: AuthMode, oauth?: OauthState | null): boolean {
  return authMode === "session" && oauth?.phase === "waiting" && oauth.kind === "oauth" && !!oauth.authorize_url;
}

export function showDeviceOverlay(oauth?: OauthState | null): boolean {
  return oauth?.phase === "waiting" && oauth.kind === "device";
}

export function deviceOpenUrl(oauth?: OauthState | null): string | undefined {
  return oauth?.verification_uri_complete || oauth?.verification_uri || undefined;
}

export function persistServerBody(input: {
  authMode: AuthMode;
  url: string;
  key: string;
  model: string;
  imageUrl: string;
  imageModel: string;
}) {
  return {
    auth_mode: authModeWire(input.authMode),
    base_url: chatOrigin(input.authMode, input.url),
    api_key: input.authMode === "session" ? undefined : input.key,
    model: input.model,
    image_base_url: input.imageUrl.trim(),
    image_model: input.imageModel.trim(),
  };
}

export function persistApplyBody(input: {
  authMode: AuthMode;
  url: string;
  key: string;
  model: string;
  imageUrl: string;
  imageModel: string;
  lossy: boolean;
  windowTok: string;
  maxSteps: string;
}) {
  return {
    ...persistServerBody(input),
    low_precision: input.lossy,
    working_window: parseTok(input.windowTok),
    max_steps: parseSteps(input.maxSteps),
  };
}

export function authCards(opts: {
  sessionState: string;
  loggedIn: boolean | null;
  keySet: boolean;
  wire: string;
}): Array<{ id: AuthMode; title: string; body: string; extra: string }> {
  return [
    {
      id: "session",
      title: "grok login 会话",
      body: "SpaceXAI OAuth（默认）或设备码，写入 ~/.grok/auth.json。",
      extra: `会话 ${sessionLabel(opts.sessionState, opts.loggedIn)}`,
    },
    {
      id: "api_key",
      title: "API key",
      body: "XAI_API_KEY 或控制台粘贴。保存后只显示是否已配置。",
      extra: opts.keySet ? "key 已保存" : "未配置 key",
    },
    {
      id: "custom",
      title: "自定义转发端点",
      body: "自建 /v1 网关。Grok 转发走 HTTP/1.1 SSE，effort 用会话设置、不强制 high；与 OAuth 同一套 Cursor Responses 体。OAuth 头只加在 grok login 上。",
      extra: wireExtra(opts.wire),
    },
  ];
}
