import type { SessionInfo } from "./api";

export function sessionBlob(s: SessionInfo): string {
  return `${s.id} ${s.title} ${s.preview} ${s.channel}`.toLowerCase();
}

export function filterSessions(rows: SessionInfo[], q: string, channel: string): SessionInfo[] {
  const needle = q.toLowerCase();
  return rows.filter((s) => {
    if (needle && !sessionBlob(s).includes(needle)) return false;
    if (channel !== "all" && (s.channel || "console") !== channel) return false;
    return true;
  });
}

export function sessionChannels(rows: SessionInfo[]): string[] {
  return [...new Set(rows.map((s) => s.channel || "console"))];
}

export function allShownPicked(shownIds: string[], picked: Set<string>): boolean {
  return shownIds.length > 0 && shownIds.every((id) => picked.has(id));
}

export function togglePicked(picked: Set<string>, id: string, on: boolean): Set<string> {
  const next = new Set(picked);
  if (on) next.add(id);
  else next.delete(id);
  return next;
}
