export type McpServer = {
  name: string;
  command: string;
  args?: string[];
  description: string;
  methods?: string[];
  cwd?: string;
  editable?: boolean;
  env_set?: boolean;
};

/** "K=V" 每行一条 → env map；忽略空行和没有 = 的行。 */
export function parseEnvLines(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    const i = t.indexOf("=");
    if (i <= 0) continue;
    out[t.slice(0, i).trim()] = t.slice(i + 1).trim();
  }
  return out;
}

export function splitMcpMethods(s: string): string[] {
  return s
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);
}

export function mcpTestText(ok: boolean, tools?: string[], error?: string | null): { ok: boolean; text: string } {
  if (ok) {
    return {
      ok: true,
      text: `连通成功 · ${(tools || []).length} 个方法${
        tools?.length ? `：${tools.slice(0, 8).join(", ")}${tools.length > 8 ? " …" : ""}` : ""
      }`,
    };
  }
  return { ok: false, text: `失败：${error || "未知错误"}` };
}
