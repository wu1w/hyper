export const CURSOR_TOOLS =
  "Read / Write / StrReplace / Delete / Glob / Grep / ReadLints / EditNotebook / Shell / WebSearch / WebFetch / GenerateImage / TodoWrite / AskQuestion / SwitchMode / Task / AwaitShell";

export type WebCfg = { enabled?: boolean; provider?: string; tavily_key_set?: boolean };

export function webProviderOf(webDesc: string, webCfg: WebCfg | null): string {
  return (
    /provider:\s*(\w+)/.exec(webDesc)?.[1] ||
    (webCfg?.provider === "tavily" || webCfg?.tavily_key_set ? "tavily" : "builtin")
  );
}

export function webEnabledOf(webCfg: WebCfg | null, hasWebTool: boolean): boolean {
  return webCfg ? !!webCfg.enabled : hasWebTool;
}

export function webEngineLabel(web: WebCfg): string {
  return web.tavily_key_set ? "Tavily（已配 key）" : "内置（Bing / DuckDuckGo，免配置）";
}

export function onOffPill(on: boolean): { cls: string; label: string } {
  return on ? { cls: "ok", label: "已启用" } : { cls: "idle", label: "未启用" };
}
