/**
 * Shown when /api/about never answered: the dashboard can't tell whether the server is read-only, so it
 * hides the actions that change things until Retry gets an answer.
 */
export function ServerSettingsBanner({ onRetry }: { onRetry: () => void }) {
  return (
    <div
      role="alert"
      className="bg-status-waiting/10 border-b border-status-waiting/30 px-4 py-2 flex items-center justify-center gap-3 text-xs font-mono text-text-primary animate-fade-in"
    >
      <span>Couldn't load this server's settings, so actions that change things are hidden.</span>
      <button
        type="button"
        onClick={onRetry}
        className="rounded border border-status-waiting/40 px-2 py-0.5 text-text-primary hover:bg-status-waiting/10 cursor-pointer"
      >
        Retry
      </button>
    </div>
  );
}
