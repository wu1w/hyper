export const WS_RECENTS_KEY = "hyper.workspace.recents";
export const WS_RECENTS_MAX = 8;

export function parseWsRecents(raw: string | null): string[] {
  if (!raw) return [];
  try {
    const j = JSON.parse(raw) as unknown;
    return Array.isArray(j) ? j.filter((x): x is string => typeof x === "string" && !!x.trim()) : [];
  } catch {
    return [];
  }
}

export function pushRecentPath(path: string, recents: string[], max = WS_RECENTS_MAX): string[] {
  const p = path.trim();
  if (!p) return recents;
  return [p, ...recents.filter((x) => x !== p)].slice(0, max);
}

export function recentLabel(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() || p;
}

export function treePadLeft(path: string): number {
  return 8 + Math.min(24, (path.split("/").length - 1) * 12);
}

export function filesLocked(applying: boolean, picking: boolean): boolean {
  return applying || picking;
}

export function openEntryAction(dir: boolean, locked: boolean): "noop" | "workspace" | "preview" {
  if (dir) return locked ? "noop" : "workspace";
  return "preview";
}
