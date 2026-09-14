export function canSavePreview(
  editable: boolean,
  office: boolean,
  officeKey: string,
  hasBytes: boolean,
): boolean {
  return editable && (office ? !!officeKey : hasBytes);
}

export function previewDockClass(opts: {
  maximized: boolean;
  office: boolean;
  layout: "chat" | "page";
}): string {
  return `pv-dock${opts.maximized ? " max" : ""}${opts.office ? " oo" : ""}${opts.layout === "chat" ? " chat" : ""}`;
}

export type OfficePoll = { ready?: boolean; starting?: boolean; hint?: string | null };

export function officeTick(
  st: OfficePoll,
  dirty: boolean,
): { reload?: boolean; note?: string; wait?: boolean } {
  if (st.ready) return dirty ? { note: officeDirtyNote() } : { reload: true };
  return { note: st.hint ? String(st.hint) : undefined, wait: !!st.starting };
}

export function truncatedPreviewNote(truncated: boolean): string {
  return truncated ? "文件超过 96MB，预览是截断后的内容，可能打不开。" : "";
}

export function officeDirtyNote(): string {
  return "文档服务已就绪。保存或关闭后重新打开即可完整编辑。";
}

export function maximizeLabel(maximized: boolean): { aria: string; title: string } {
  return maximized
    ? { aria: "还原预览", title: "还原" }
    : { aria: "最大化预览", title: "最大化" };
}
