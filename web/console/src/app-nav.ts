export type PageId =
  | "chat"
  | "inbox"
  | "channels"
  | "sessions"
  | "cron"
  | "heartbeat"
  | "files"
  | "skills"
  | "mcp"
  | "tools"
  | "settings"
  | "security"
  | "usage";

export const NAV: Array<{
  group: string;
  items: Array<{ id: PageId; label: string; icon: string; badge?: boolean }>;
}> = [
  {
    group: "主页",
    items: [
      { id: "chat", label: "聊天", icon: "chat" },
      { id: "channels", label: "频道", icon: "radio" },
      { id: "files", label: "文件", icon: "folder" },
      { id: "cron", label: "定时任务", icon: "clock" },
      { id: "heartbeat", label: "心跳", icon: "pulse" },
    ],
  },
  {
    group: "工作区",
    items: [
      { id: "inbox", label: "收件箱", icon: "shield", badge: true },
      { id: "sessions", label: "会话", icon: "list" },
      { id: "skills", label: "技能", icon: "spark" },
      { id: "mcp", label: "MCP", icon: "plug" },
      { id: "tools", label: "工具", icon: "wrench" },
    ],
  },
];

export const FOOT: Array<{ id: PageId; label: string; icon: string }> = [
  { id: "settings", label: "模型", icon: "cpu" },
  { id: "security", label: "安全", icon: "lock" },
  { id: "usage", label: "用量", icon: "chart" },
];

export const TITLES: Record<PageId, string> = {
  chat: "聊天",
  inbox: "收件箱",
  channels: "频道",
  sessions: "会话",
  cron: "定时任务",
  heartbeat: "心跳",
  files: "文件",
  skills: "技能",
  mcp: "MCP",
  tools: "工具",
  settings: "模型",
  security: "安全",
  usage: "用量",
};

export function isPageId(id: string): id is PageId {
  return id in TITLES;
}

export function pageFromHash(hash = ""): PageId {
  const h = hash.replace(/^#/, "");
  if (isPageId(h)) return h;
  return "chat";
}

export function detailsOpenFromStore(saved: string | null, wide: boolean): boolean {
  if (saved === "1") return true;
  if (saved === "0") return false;
  return wide;
}

export function modelLinkLabel(opts: { linked: boolean; probing: boolean; model: string }): string {
  if (opts.linked) return opts.model ? `模型可达 · ${opts.model}` : "模型可达";
  if (opts.probing) return "检测中";
  return "模型不可达";
}

export function linkChipClass(ok: boolean | null): string {
  if (ok === true) return "chip link-chip";
  if (ok === false) return "chip link-chip bad";
  return "chip link-chip";
}

export function linkDotClass(ok: boolean | null): string {
  if (ok === true) return "dot";
  if (ok === null) return "dot wait";
  return "dot off";
}
