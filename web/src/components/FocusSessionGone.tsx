/** Fills a focus-mode window whose session was trashed or deleted: such a window cannot fall back to the dashboard.
 *  `title` is the session's last known name, null when this window never saw it. */
export function FocusSessionGone({
  title,
  trashed,
  onExit,
}: {
  title: string | null;
  trashed: boolean;
  onExit: () => void;
}) {
  const name = title ? <span className="font-mono text-text-primary">{title}</span> : "This Session";
  return (
    <div
      data-testid="focus-session-gone"
      className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center text-text-secondary"
    >
      <p className="text-sm">
        {name} {trashed ? "was moved to Trash." : "no longer exists."}
      </p>
      <button
        type="button"
        onClick={onExit}
        className="rounded-md px-3 py-1.5 text-sm text-text-primary bg-surface-700/50 hover:bg-surface-700 cursor-pointer transition-colors"
      >
        Show all Sessions
      </button>
    </div>
  );
}
