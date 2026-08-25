import { useEffect, useState } from "react";
import type { PreviewProps } from "./kinds";
import { copyU8, mimeFromPath } from "./bytes";

export function ImagePreview({ path, bytes, registerExport }: PreviewProps) {
  const [src, setSrc] = useState("");
  useEffect(() => {
    registerExport(async () => copyU8(bytes));
  }, [bytes, registerExport]);
  useEffect(() => {
    const blob = new Blob([copyU8(bytes)], { type: mimeFromPath(path) });
    const obj = URL.createObjectURL(blob);
    setSrc(obj);
    return () => URL.revokeObjectURL(obj);
  }, [bytes, path]);
  if (!src) return <div className="sub">加载图片…</div>;
  return <img className="pv-img" src={src} alt={path} />;
}
