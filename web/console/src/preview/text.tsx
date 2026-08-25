import { useEffect, useRef } from "react";
import type { PreviewProps } from "./kinds";

export function TextPreview({ bytes, onDirty, registerExport }: PreviewProps) {
  const initial = new TextDecoder().decode(bytes);
  const ta = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    registerExport(async () => new TextEncoder().encode(ta.current?.value ?? initial));
  }, [registerExport, initial]);
  return (
    <textarea
      className="pv-text"
      defaultValue={initial}
      spellCheck={false}
      onChange={(e) => onDirty(e.target.value !== initial)}
      ref={ta}
    />
  );
}
