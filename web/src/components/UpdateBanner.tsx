import type { UpdateStatus } from "../lib/api";

/** Top-of-app banner offering the update `useUpdateBanner` found. */
export function UpdateBanner({ status, onDismiss }: { status: UpdateStatus; onDismiss: () => void }) {
  return (
    <div
      role="status"
      aria-label={`Update available: v${status.latest_version}`}
      className="bg-brand-600/10 border-b border-brand-600/30 px-4 py-2 flex items-center justify-center gap-3 text-xs font-mono text-brand-300 animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-brand-400 shrink-0" />
      <span>
        Update available: v{status.current_version} → v{status.latest_version}.
      </span>
      {status.release_url && (
        <a
          href={status.release_url}
          target="_blank"
          rel="noopener noreferrer"
          className="underline hover:text-brand-200"
        >
          Release notes
        </a>
      )}
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss update notice"
        className="ml-2 text-text-muted hover:text-text-secondary cursor-pointer text-base leading-none px-1"
      >
        &times;
      </button>
    </div>
  );
}
