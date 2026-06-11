interface Props {
  onClick: () => void;
}

const IS_MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/.test(navigator.platform);

export function PaletteTriggerPill({ onClick }: Props) {
  const modKey = IS_MAC ? "⌘" : "Ctrl";

  return (
    <>
      {/* Desktop pill */}
      <button
        onClick={onClick}
        className="hidden sm:flex items-center gap-2 h-8 w-full max-w-[420px] px-3 bg-surface-900 border border-surface-700/60 rounded-md cursor-pointer hover:border-surface-700 hover:bg-surface-850 transition-colors text-text-muted"
        aria-label="Open command palette"
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          className="shrink-0"
        >
          <circle cx="11" cy="11" r="7" />
          <line x1="21" y1="21" x2="16.65" y2="16.65" />
        </svg>
        <span className="flex-1 text-left text-[13px]">Search anything…</span>
        <kbd className="font-mono text-[10px] px-1.5 py-0.5 rounded bg-surface-800 border border-surface-700 text-text-muted">
          {modKey}K
        </kbd>
      </button>

      {/* Mobile icon-only */}
      <button
        onClick={onClick}
        className="sm:hidden w-8 h-8 flex items-center justify-center rounded-md cursor-pointer text-text-muted hover:text-text-secondary hover:bg-surface-700/50 transition-colors"
        aria-label="Open command palette"
      >
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <circle cx="11" cy="11" r="7" />
          <line x1="21" y1="21" x2="16.65" y2="16.65" />
        </svg>
      </button>
    </>
  );
}
