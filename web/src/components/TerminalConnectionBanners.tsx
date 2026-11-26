import { useEffect, useState } from "react";
import { useConnectionBanner } from "../hooks/useConnectionBanner";

// Connection banners for every terminal surface: Reconnecting while the socket redials, and a manual retry once
// the budget is spent.

/** A phone drops its sockets on every resume and redials at once; only a drop that outlasts this is shown. */
const RECONNECTING_GRACE_MS = 2000;

interface Props {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  maxRetries: number;
  onRetry: () => void;
  /** The pane's process exited; Restart re-ensures the session instead of just redialing. */
  paneDead?: boolean;
  onRestart?: () => void;
}

export function TerminalConnectionBanners({
  connected,
  reconnecting,
  retryCount,
  maxRetries,
  onRetry,
  paneDead,
  onRestart,
}: Props) {
  const daemon = useConnectionBanner();
  const [slow, setSlow] = useState(false);
  useEffect(() => {
    if (!reconnecting) return;
    const timer = setTimeout(() => setSlow(true), RECONNECTING_GRACE_MS);
    return () => {
      clearTimeout(timer);
      setSlow(false);
    };
  }, [reconnecting]);
  if (connected) return null;
  if (paneDead) {
    return (
      <div className="bg-status-error/10 border-b border-status-error/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
        <span className="text-xs text-status-error">Agent exited</span>
        {onRestart && (
          <button onClick={onRestart} className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer underline">
            Restart
          </button>
        )}
      </div>
    );
  }
  if (reconnecting) {
    // The app's Daemon unreachable banner already says it for every pane.
    if (!slow || daemon === "down") return null;
    return (
      <div className="bg-status-waiting/15 border-b border-status-waiting/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
        <span className="text-xs text-status-waiting">Reconnecting…</span>
      </div>
    );
  }
  if (retryCount >= maxRetries) {
    return (
      <div className="bg-status-error/10 border-b border-status-error/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
        <span className="text-xs text-status-error">Connection lost</span>
        <button onClick={onRetry} className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer underline">
          Retry
        </button>
      </div>
    );
  }
  return null;
}
