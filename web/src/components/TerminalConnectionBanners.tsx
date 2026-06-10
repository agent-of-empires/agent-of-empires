// Shared connection-state banners for every terminal surface (live
// views and desktop xterm views render identical chrome here): the
// retry countdown while the WS redials, and the manual-retry strip once
// the budget is exhausted.

interface Props {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  maxRetries: number;
  onRetry: () => void;
}

export function TerminalConnectionBanners({
  connected,
  reconnecting,
  retryCount,
  retryCountdown,
  maxRetries,
  onRetry,
}: Props) {
  if (connected) return null;
  if (reconnecting) {
    return (
      <div className="bg-status-waiting/15 border-b border-status-waiting/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
        <span className="text-xs text-status-waiting">
          Reconnecting in {retryCountdown}s... ({retryCount}/{maxRetries})
        </span>
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
