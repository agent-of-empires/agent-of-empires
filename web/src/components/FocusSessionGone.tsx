/** Fills a focus-mode window whose session no longer exists: such a window cannot fall back to the dashboard. */
export function FocusSessionGone({ onExit }: { onExit: () => void }) {
  return (
    <div
      data-testid="focus-session-gone"
      className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center text-text-secondary"
    >
      <p className="text-sm">This session no longer exists.</p>
      <button
        type="button"
        onClick={onExit}
        className="rounded-md px-3 py-1.5 text-sm text-text-primary bg-surface-700/50 hover:bg-surface-700 cursor-pointer transition-colors"
      >
        Exit focus mode
      </button>
    </div>
  );
}
