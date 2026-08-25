/** Preview kind registry. Each editor is lazy-loaded so pdf.js stays off the chat path. */

import type { ComponentType } from "react";

export type PreviewKindId =
  | "browser"
  | "image"
  | "text"
  | "word"
  | "sheet"
  | "ppt"
  | "pdf"
  | "canvas"
  | "visio";

export type PreviewProps = {
  path: string;
  bytes: Uint8Array;
  url: string;
  onDirty: (dirty: boolean) => void;
  registerExport: (fn: () => Promise<Uint8Array>) => void;
  onOfficeKey?: (key: string) => void;
};

export type PreviewKind = {
  id: PreviewKindId;
  label: string;
  match: (path: string) => boolean;
  editable: boolean;
  hint?: string;
  load: () => Promise<ComponentType<PreviewProps>>;
};

function ext(path: string): string {
  const n = (path.split(/[\\/]/).pop() || "").toLowerCase();
  if (n.endsWith(".canvas.json")) return ".canvas.json";
  const i = n.lastIndexOf(".");
  return i >= 0 ? n.slice(i) : "";
}

export const KINDS: PreviewKind[] = [
  {
    id: "browser",
    label: "浏览器",
    match: (p) => [".html", ".htm"].includes(ext(p)),
    editable: true,
    load: () => import("./browser").then((m) => m.BrowserPreview),
  },
  {
    id: "image",
    label: "图片",
    match: (p) => [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp"].includes(ext(p)),
    editable: false,
    load: () => import("./image").then((m) => m.ImagePreview),
  },
  {
    id: "word",
    label: "Word",
    match: (p) => [".docx", ".doc", ".docm", ".odt"].includes(ext(p)),
    editable: true,
    hint: "OnlyOffice 编辑，保留公式与原格式。",
    load: () => import("./word").then((m) => m.WordPreview),
  },
  {
    id: "sheet",
    label: "表格",
    match: (p) => [".xlsx", ".xlsm", ".xls", ".csv", ".ods"].includes(ext(p)),
    editable: true,
    hint: "OnlyOffice 表格（公式、多表、大表）。",
    load: () => import("./sheet").then((m) => m.SheetPreview),
  },
  {
    id: "ppt",
    label: "PPT",
    match: (p) => [".pptx", ".ppt", ".ppsx", ".pptm", ".odp"].includes(ext(p)),
    editable: true,
    hint: "OnlyOffice 幻灯片。",
    load: () => import("./ppt").then((m) => m.PptPreview),
  },
  {
    id: "pdf",
    label: "PDF",
    match: (p) => ext(p) === ".pdf",
    editable: true,
    hint: "OnlyOffice 可填表、批注、改 PDF。",
    load: () => import("./pdf").then((m) => m.PdfPreview),
  },
  {
    id: "canvas",
    label: "画布",
    match: (p) => ext(p) === ".canvas.json",
    editable: true,
    load: () => import("./canvas").then((m) => m.CanvasPreview),
  },
  {
    id: "visio",
    label: "Visio",
    match: (p) => [".vsd", ".vsdx"].includes(ext(p)),
    editable: false,
    hint: "vsdx 显示页面大纲；旧版 .vsd 是二进制，请另存为 vsdx。",
    load: () => import("./visio").then((m) => m.VisioPreview),
  },
  {
    id: "text",
    label: "文本",
    match: () => true,
    editable: true,
    load: () => import("./text").then((m) => m.TextPreview),
  },
];

export function kindFor(path: string): PreviewKind {
  return KINDS.find((k) => k.match(path)) || KINDS[KINDS.length - 1];
}

export function isOfficeKind(id: PreviewKindId): boolean {
  return id === "word" || id === "sheet" || id === "ppt" || id === "pdf";
}

export function isOfficePath(path: string): boolean {
  const id = kindFor(path).id;
  return id !== "text" || /\.(md|txt|json|csv|xml|svg)$/i.test(path);
}
