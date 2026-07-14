interface Props {
  sidebarOpen: boolean;
  onToggle: () => void;
}

/** Mobile shortcut for the session sidebar, mirroring the keyboard FAB so a
 *  deep-in-a-session thumb can reach the sidebar without stretching to the
 *  top-bar toggle (#2245). Sits bottom-left, opposite the keyboard FAB. */
export function SidebarFab({ sidebarOpen, onToggle }: Props) {
  return (
    <button
      type="button"
      aria-label={sidebarOpen ? "Close sidebar" : "Open sidebar"}
      onClick={onToggle}
      // Same reason as the keyboard FAB: a button steals focus on pointer-down,
      // which would blur the terminal input and drop the soft keyboard before
      // onClick runs. Preventing the default keeps the keyboard up so the
      // sidebar can be toggled while typing. onClick still fires.
      onMouseDown={(e) => e.preventDefault()}
      className="absolute left-3 bottom-3 z-10 w-10 h-10 rounded-full bg-surface-800/90 border border-surface-700/30 text-text-secondary flex items-center justify-center shadow-lg backdrop-blur-sm active:scale-95"
    >
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
        <rect x="3" y="3" width="18" height="18" rx="2" />
        <line x1="9" y1="3" x2="9" y2="21" />
      </svg>
    </button>
  );
}
