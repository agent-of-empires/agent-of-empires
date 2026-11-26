import { Plus, Search } from "lucide-react";

import { OFFLINE_TITLE } from "../../lib/connectionState";
import { formatShortcut, IS_MAC, SHORTCUTS_BY_ID } from "../../lib/shortcuts";

const row =
  "w-full flex items-center gap-2.5 h-8 px-2 rounded-md text-[13px] text-text-secondary cursor-pointer transition-colors duration-75 hover:bg-surface-800 hover:text-text-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500 disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:bg-transparent";

/** The sidebar's top actions, as Claude Code desktop and Conductor list them: a new session, then search. */
export function SidebarNav({
  compact,
  offline,
  onNew,
  onSearch,
}: {
  compact: boolean;
  offline: boolean;
  onNew: () => void;
  /** Omitted where the command palette is unavailable. */
  onSearch?: () => void;
}) {
  const searchKeys = formatShortcut(SHORTCUTS_BY_ID.palette.chord, { mac: IS_MAC, separator: "+" });
  return (
    <nav
      aria-label="Sidebar actions"
      className={`max-md:hidden flex flex-col gap-0.5 ${compact ? "px-1" : "px-2"} pt-2`}
    >
      <button
        type="button"
        onClick={onNew}
        disabled={offline}
        title={offline ? OFFLINE_TITLE : undefined}
        data-testid="sidebar-new-session"
        className={`${row} ${compact ? "justify-center" : ""}`}
        aria-label={compact ? "New session" : undefined}
      >
        <Plus className="size-4 shrink-0" strokeWidth={1.5} aria-hidden />
        {!compact && <span>New session</span>}
      </button>
      {onSearch && (
        <button
          type="button"
          onClick={onSearch}
          data-testid="sidebar-search"
          className={`${row} ${compact ? "justify-center" : ""}`}
          aria-label={compact ? "Search" : undefined}
        >
          <Search className="size-4 shrink-0" strokeWidth={1.5} aria-hidden />
          {!compact && (
            <>
              <span className="flex-1 text-left">Search</span>
              {/* A shortcut means nothing on a touch screen. */}
              <kbd className="font-mono text-[10px] text-text-dim pointer-coarse:hidden">{searchKeys}</kbd>
            </>
          )}
        </button>
      )}
    </nav>
  );
}
