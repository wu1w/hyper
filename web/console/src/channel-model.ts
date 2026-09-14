import type { ChannelEp, ChannelKind } from "./api";

export const DM_POLICY = [
  { id: "open", label: "开放" },
  { id: "allowlist", label: "白名单" },
  { id: "closed", label: "关闭" },
];
export const GROUP_POLICY = [
  { id: "open", label: "开放" },
  { id: "allowlist", label: "白名单" },
  { id: "mention", label: "需提及" },
  { id: "closed", label: "关闭" },
];

export function runtimePill(e: ChannelEp): { cls: string; label: string } | null {
  const st = e.runtime?.state;
  if (!st) return null;
  if (st === "running") return { cls: "ok", label: "运行中" };
  if (st === "retry") return { cls: "warn", label: "重试中" };
  if (st === "error") return { cls: "err", label: "连接错误" };
  if (st === "no_credentials") return { cls: "warn", label: "缺凭证" };
  if (e.enabled) return { cls: "idle", label: "未连接" };
  return null;
}

export function policyName(kind: "dm" | "group", id?: string) {
  const fallback = kind === "dm" ? "allowlist" : "mention";
  const rows = kind === "dm" ? DM_POLICY : GROUP_POLICY;
  return rows.find((r) => r.id === (id || fallback))?.label || id || (kind === "dm" ? "白名单" : "需提及");
}

export function matchLine(e: ChannelEp) {
  const bits = [`私信 ${policyName("dm", e.dm_policy)}`, `群聊 ${policyName("group", e.group_policy)}`];
  if (e.require_mention && e.group_policy !== "mention") bits.push("群需@");
  const n = (e.allow_from || []).length;
  if (n) bits.push(`白名单 ${n}`);
  const d = (e.deny_from || []).length;
  if (d) bits.push(`拒绝 ${d}`);
  return bits.join(" · ");
}

export function extraRecord(extra?: Record<string, unknown>): Record<string, string> {
  const out: Record<string, string> = {};
  if (!extra) return out;
  for (const [k, v] of Object.entries(extra)) {
    if (typeof v === "string") out[k] = v;
  }
  return out;
}

export function extraStr(e: ChannelEp, key: string): string {
  const v = e.extra?.[key];
  return typeof v === "string" ? v : "";
}

export function nextEpId(eps: ChannelEp[], kind: string) {
  if (!eps.some((e) => e.id === kind)) return kind;
  let i = 2;
  while (eps.some((e) => e.id === `${kind}-${i}`)) i += 1;
  return `${kind}-${i}`;
}

export function toChannelPayload(eps: ChannelEp[]) {
  return eps.map((e) => {
    const extra = extraRecord(e.extra);
    if (e.kind === "telegram" && e.bot_token?.trim()) extra.bot_token = e.bot_token.trim();
    return {
      id: e.id.trim() || nextEpId(eps, e.kind),
      kind: e.kind,
      enabled: !!e.enabled,
      bind: e.bind || "",
      reply_url: e.reply_url || "",
      require_mention: e.require_mention !== false,
      dm_policy: e.dm_policy || "allowlist",
      group_policy: e.group_policy || "mention",
      allow_from: e.allow_from || [],
      deny_from: e.deny_from || [],
      secret: e.secret || "",
      extra,
    };
  });
}

export function kindSpec(catalog: ChannelKind[], kind: string): ChannelKind | undefined {
  return catalog.find((c) => c.id === kind);
}

export function isBound(e: ChannelEp, spec?: ChannelKind) {
  if (e.bot_token_set || e.secret_set) return true;
  const set = new Set(e.creds_set || []);
  if (spec?.fields.some((f) => f.secret && set.has(f.key))) return true;
  return set.size > 0 && !!spec?.qr;
}

export function addableKinds(catalog: ChannelKind[], eps: ChannelEp[]): ChannelKind[] {
  const configured = new Set(eps.map((e) => e.kind));
  return catalog.filter((c) => c.in_process && (!c.once || !configured.has(c.id)));
}

export function newChannelRow(kind: string, eps: ChannelEp[]): ChannelEp {
  const im = kind !== "webhook";
  return {
    id: nextEpId(eps, kind),
    kind,
    enabled: false,
    require_mention: im,
    dm_policy: im ? "allowlist" : "open",
    group_policy: im ? "mention" : "open",
    bind: kind === "webhook" ? "127.0.0.1:8788" : "",
    extra: kind === "feishu" ? { domain: "feishu" } : {},
    _local: true,
  };
}

export function mergeQrCreds(cur: ChannelEp, creds: Record<string, string>): ChannelEp {
  const extra = { ...extraRecord(cur.extra) };
  for (const [k, v] of Object.entries(creds)) {
    if (v) extra[k] = v;
  }
  return { ...cur, extra, enabled: true, _local: false };
}

export function upsertBody(row: ChannelEp) {
  const payload = toChannelPayload([row])[0];
  const orig = (row._origId || "").trim();
  return {
    upsert: payload,
    rename: orig && orig !== payload.id ? orig : undefined,
  };
}

export function qrPollGap(kind: string) {
  if (kind === "feishu") return 5000;
  if (kind === "qq" || kind === "wecom" || kind === "dingtalk") return 3000;
  return 1500;
}

export function qrStatusLabel(status: string, kind?: string, live?: boolean) {
  if (status === "waiting") {
    if (kind === "qq") return "等待扫码 · QQ 开通机器人可能要一会儿";
    if (kind === "dingtalk") return "等待扫码 · 钉钉可能要创建/发布应用";
    return "等待扫码";
  }
  if (status === "scanned") return "已扫，请在手机上确认";
  if (status === "success") return live ? "凭证已写入，正在连接" : "凭证已写入";
  if (status === "expired") return "二维码已过期";
  if (status === "fail") return "授权失败";
  return status;
}

export function channelEnableHint(spec?: ChannelKind): string {
  if (!spec?.in_process) return "凭证写入 config.toml。该平台消息适配器尚未进进程。";
  if (spec.id === "qq") return "扫码成功后本进程会连 QQ 官方网关，手机「连接中」才会结束";
  if (spec.id === "wechat") return "扫码成功后本进程会 iLink 长轮询，才能收到微信消息";
  if (spec.id === "wecom") return "扫码成功后本进程会连企微 WebSocket";
  if (spec.id === "dingtalk") return "扫码成功后本进程会连钉钉 Stream";
  if (spec.id === "feishu") return "扫码成功后本进程会连飞书长连接";
  if (spec.id === "telegram") return "保存后本进程会 long-poll Bot API";
  if (spec.id === "webhook") return "保存后本进程会在 bind 地址接听 POST /inbound";
  return "保存后本进程会连接官方接口";
}

export function cardStatus(e: ChannelEp): { cls: string; label: string } {
  const rt = runtimePill(e);
  if (rt) return { cls: rt.cls, label: rt.label };
  return e.enabled ? { cls: "ok", label: "已启用" } : { cls: "idle", label: "未启用" };
}

export function addableAction(c: ChannelKind, configured: boolean): string {
  if (configured && !c.once) return "再添加";
  return c.qr ? "扫码" : "添加";
}

export function parseTagDraft(raw: string): string[] {
  return raw
    .split(/[,，\s]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

export function mergeTags(values: string[], parts: string[]): string[] {
  const next = [...values];
  for (const p of parts) if (!next.includes(p)) next.push(p);
  return next;
}

export function qrFailMessage(status: string, reason?: string): string {
  if (status === "expired") return "二维码已过期，请重新获取";
  return reason || "授权失败";
}

export function qrPollDone(status: string): boolean {
  return status === "success" || status === "fail" || status === "expired";
}

export function channelCardClass(selected: boolean, enabled: boolean): string {
  return `card ch-card${selected ? " on" : ""}${enabled ? "" : " dim"}`;
}

export function qrButtonLabel(hasImage: boolean): string {
  return hasImage ? "刷新二维码" : "获取二维码";
}

export type AddChannelPlan = { action: "ignore" } | { action: "open"; index: number } | { action: "create" };

export function addChannelPlan(kind: string, catalog: ChannelKind[], eps: ChannelEp[]): AddChannelPlan {
  const spec = kindSpec(catalog, kind);
  if (!spec?.in_process) return { action: "ignore" };
  if (spec.once) {
    const existing = eps.findIndex((e) => e.kind === kind);
    if (existing >= 0) return { action: "open", index: existing };
  }
  return { action: "create" };
}
