import { useEffect, useRef } from "react";
import type { PreviewProps } from "./kinds";

export function BrowserPreview({ path, bytes, url, onDirty, registerExport }: PreviewProps) {
  const ta = useRef<HTMLTextAreaElement>(null);
  const initial = new TextDecoder().decode(bytes);
  useEffect(() => {
    registerExport(async () => new TextEncoder().encode(ta.current?.value ?? initial));
  }, [registerExport, initial]);
  return (
    <div className="pv-split">
      <iframe className="pv-frame" title={path} src={url} sandbox="allow-scripts allow-same-origin" />
      <textarea
        className="pv-html-src"
        defaultValue={initial}
        spellCheck={false}
        aria-label="HTML 源码"
        onChange={(e) => onDirty(e.target.value !== initial)}
        ref={ta}
      />
    </div>
  );
}
