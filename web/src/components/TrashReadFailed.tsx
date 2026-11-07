/** Fills the main pane when an opened session is absent from the list and the Trash read that could hold it failed:
 *  the session is unread, not gone. */
export function TrashReadFailed({ onRetry }: { onRetry: () => void }) {
  return (
    <div
      data-testid="trash-read-failed"
      role="alert"
      className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center text-text-secondary"
    >
      <p className="text-sm">Could not load this Session. Trash could not be read.</p>
      <button
        type="button"
        onClick={onRetry}
        className="rounded-md px-3 py-1.5 text-sm text-text-primary bg-surface-700/50 hover:bg-surface-700 cursor-pointer transition-colors"
      >
        Retry
      </button>
    </div>
  );
}
