interface Props {
  keyboardOpen: boolean;
  onToggle: () => void;
}

export function KeyboardFab({ keyboardOpen, onToggle }: Props) {
  return (
    <button
      type="button"
      aria-label={keyboardOpen ? "Close keyboard" : "Open keyboard"}
      onClick={onToggle}
      // Keep focus where it is: a button steals focus on pointer-down,
      // which would blur the terminal input BEFORE onClick runs and turn
      // every "close keyboard" tap into a re-open. onClick still fires.
      onMouseDown={(e) => e.preventDefault()}
      className="absolute right-3 bottom-3 z-10 w-10 h-10 rounded-full bg-surface-800/90 border border-surface-700/30 text-text-secondary flex items-center justify-center shadow-lg backdrop-blur-sm active:scale-95"
    >
      {keyboardOpen ? (
        <svg
          width="18"
          height="18"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <rect x="1" y="1" width="22" height="16" rx="2" />
          <line x1="5" y1="13" x2="19" y2="13" />
          <line x1="8" y1="20" x2="16" y2="20" />
          <line x1="12" y1="17" x2="12" y2="20" />
        </svg>
      ) : (
        <svg
          width="18"
          height="14"
          viewBox="0 0 24 18"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <rect x="1" y="1" width="22" height="16" rx="2" />
          <line x1="5" y1="13" x2="19" y2="13" />
          <line x1="5" y1="9" x2="5.01" y2="9" />
          <line x1="9" y1="9" x2="9.01" y2="9" />
          <line x1="13" y1="9" x2="13.01" y2="9" />
          <line x1="17" y1="9" x2="17.01" y2="9" />
          <line x1="5" y1="5" x2="5.01" y2="5" />
          <line x1="9" y1="5" x2="9.01" y2="5" />
          <line x1="13" y1="5" x2="13.01" y2="5" />
          <line x1="17" y1="5" x2="17.01" y2="5" />
        </svg>
      )}
    </button>
  );
}
