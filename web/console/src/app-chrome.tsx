import { type ReactNode } from "react";
import type { PageId } from "./app-nav";
import { Icon } from "./ui";

function isWinDesktop() {
  return window.grokHyperDesktop?.platform === "win32";
}

function WinCaptionIcon({ kind }: { kind: "min" | "max" | "close" }) {
  if (kind === "min") {
    return (
      <svg viewBox="0 0 10 10" aria-hidden>
        <rect x="1" y="4.5" width="8" height="1" rx="0.2" />
      </svg>
    );
  }
  if (kind === "max") {
    return (
      <svg viewBox="0 0 10 10" aria-hidden>
        <rect x="1.5" y="1.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 10 10" aria-hidden>
      <path d="M2 2 L8 8 M8 2 L2 8" fill="none" stroke="currentColor" strokeWidth="1.15" />
    </svg>
  );
}

export function WindowButtons() {
  const desktop = window.grokHyperDesktop;
  if (!desktop) {
    return (
      <div className="traffic" aria-hidden>
        <span className="tl-r" />
        <span className="tl-y" />
        <span className="tl-g" />
      </div>
    );
  }
  const win = desktop.platform === "win32";
  return (
    <div className="traffic">
      {win ? (
        <>
          <button type="button" className="tl-y" aria-label="最小化" onClick={() => desktop.minimize()}>
            <WinCaptionIcon kind="min" />
          </button>
          <button type="button" className="tl-g" aria-label="最大化" onClick={() => desktop.toggleMaximize()}>
            <WinCaptionIcon kind="max" />
          </button>
          <button type="button" className="tl-r" aria-label="关闭" onClick={() => desktop.close()}>
            <WinCaptionIcon kind="close" />
          </button>
        </>
      ) : (
        <>
          <button type="button" className="tl-r" aria-label="关闭" onClick={() => desktop.close()} />
          <button type="button" className="tl-y" aria-label="最小化" onClick={() => desktop.minimize()} />
          <button type="button" className="tl-g" aria-label="最大化" onClick={() => desktop.toggleMaximize()} />
        </>
      )}
    </div>
  );
}

export function KeepPane({
  id,
  page,
  seen,
  children,
}: {
  id: PageId;
  page: PageId;
  seen: Set<PageId>;
  children: ReactNode;
}) {
  if (page !== id && !seen.has(id)) return null;
  return (
    <div className="main-pane" hidden={page !== id} aria-hidden={page !== id}>
      {children}
    </div>
  );
}

export function Titlebar({
  pageTitle,
  children,
}: {
  pageTitle: string;
  children: ReactNode;
}) {
  return (
    <header
      className={`titlebar${isWinDesktop() ? " win" : ""}`}
      onDoubleClick={() => window.grokHyperDesktop?.toggleMaximize()}
    >
      {isWinDesktop() ? null : <WindowButtons />}
      <div className="titlebar-title">
        <span className="doc-title">grok-hyper 控制台</span>
        <span className="doc-sub"> · {pageTitle}</span>
      </div>
      <div className="spacer" />
      <div className="no-drag">{children}</div>
      {isWinDesktop() ? <WindowButtons /> : null}
    </header>
  );
}

export { isWinDesktop };
