import { type KeyboardEvent as ReactKeyboardEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Command, defaultFilter } from "cmdk";
import { StatusGlyph } from "../StatusGlyph";
import { CheatOverlay } from "./CheatOverlay";
import { GROUP_ORDER, TAB_ORDER, type PaletteTab } from "./groups";
import type { CommandAction, CommandActionGroup } from "./types";
import { matchCheat, type CheatEffect } from "../../lib/cheats";
import { reportInfo } from "../../lib/toastBus";

// The cmdk item value: id plus the searchable text. Kept in one place so the
// pre-filter that drives tab/count visibility and the <Command.Item value=...>
// that cmdk actually scores stay in sync.
function actionValue(a: CommandAction): string {
  return `${a.id} ${[a.title, a.subtitle ?? "", ...(a.keywords ?? [])].join(" ")}`;
}

// cmdk's fuzzy scorer keeps any nonzero score, but a short query like "test"
// scatter-matches across a row's id + keywords (t·e·s·t picked from
// "acTion nEw seSsion sTart") for a tiny ~0.15 score, surfacing unrelated
// rows. Real substring / prefix / acronym hits score ~0.9+, so require a
// floor well above the scatter band. Tune here if legitimate fuzzy matches
// start dropping.
const MIN_SCORE = 0.3;

// The single scoring predicate for a row against the current query, used both
// to pre-filter groups (tab/count visibility) and as the <Command> filter so
// the two never disagree. Conversation hits are matched server-side by content
// the client text lacks, so force-keep them; an empty query keeps everything.
function scoreValue(value: string, search: string): number {
  if (value.startsWith("conversation:")) return 1;
  if (!search) return 1;
  const score = defaultFilter!(value, search) ?? 0;
  return score >= MIN_SCORE ? score : 0;
}

function matches(a: CommandAction, search: string): boolean {
  return scoreValue(actionValue(a), search) > 0;
}

interface Props {
  open: boolean;
  onClose: () => void;
  actions: CommandAction[];
  /** Called with the current search text (debounced upstream) so the host
   *  can run an async conversation-content search. */
  onSearchChange?: (query: string) => void;
  /** True while a conversation-content search is in flight; renders a
   *  spinner row in the Conversations group. */
  searching?: boolean;
}

export function CommandPalette({ open, onClose, actions, onSearchChange, searching }: Props) {
  const inputRef = useRef<HTMLInputElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const [search, setSearch] = useState("");
  // JetBrains "Search Everywhere"-style tabs: "All" shows every group (the
  // default flat view), a category tab scopes the list to one group.
  const [activeTab, setActiveTab] = useState<PaletteTab>("All");
  // Active easter-egg effect plus a monotonic id so retyping the same cheat
  // replays the animation (the id is the overlay's React key).
  const [cheat, setCheat] = useState<{ effect: CheatEffect; id: number } | null>(null);

  // A full-string match on a known Age of Empires cheat code fires a toast and
  // a one-off visual, then clears the input. Anything else is a normal search.
  const handleSearchChange = (value: string) => {
    const hit = matchCheat(value);
    if (hit) {
      reportInfo(hit.toast);
      setCheat((prev) => ({ effect: hit.effect, id: (prev?.id ?? 0) + 1 }));
      setSearch("");
      onSearchChange?.("");
      return;
    }
    setSearch(value);
    onSearchChange?.(value);
  };

  // Capture the launcher before moving focus into the palette, then restore
  // it on close so Esc / backdrop-close return keyboard users to where they
  // were instead of dropping focus on <body>. autoFocus cannot restore focus,
  // and capturing in a post-commit effect would already see the input.
  useEffect(() => {
    if (!open) return;
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    const t = setTimeout(() => inputRef.current?.focus(), 0);
    return () => {
      clearTimeout(t);
      const prev = previousFocusRef.current;
      if (prev?.isConnected) prev.focus();
    };
  }, [open]);

  // Controlled input keeps its value across open/close, so reset it when the
  // palette closes; also drop any in-flight cheat so reopening does not replay
  // the last one. Adjusting during render on the open->closed edge is the
  // React-recommended pattern, no effect needed.
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (!open) {
      setSearch("");
      setCheat(null);
      setActiveTab("All");
    }
  }

  // Stable so the overlay's cleanup timer is not reset by unrelated re-renders
  // (e.g. the user typing again while an effect is still on screen).
  const clearCheat = useCallback(() => setCheat(null), []);

  // Group only the rows that survive cmdk's search filter, so tab visibility
  // and the footer count reflect what actually renders for the current query
  // (not the raw group sizes). Mirrors the same value string and force-keep
  // rule the <Command> filter uses below.
  const grouped = useMemo(() => {
    const map = new Map<CommandActionGroup, CommandAction[]>();
    for (const g of GROUP_ORDER) map.set(g, []);
    for (const a of actions) {
      const arr = map.get(a.group);
      if (arr && matches(a, search)) arr.push(a);
    }
    return map;
  }, [actions, search]);

  // Tabs to render: "All" always, plus any group that has rows for the current
  // query. The Conversations tab also shows while a content search is in
  // flight so it is reachable before the first hit lands.
  const tabs = useMemo(
    () =>
      TAB_ORDER.filter(
        (t) => t === "All" || (grouped.get(t)?.length ?? 0) > 0 || (t === "Conversations" && !!searching),
      ),
    [grouped, searching],
  );

  // The active tab can go stale when the query changes out from under it (its
  // group emptied). Fall back to "All" rather than render an empty scope.
  if (activeTab !== "All" && !tabs.includes(activeTab)) setActiveTab("All");

  const visibleGroups = activeTab === "All" ? GROUP_ORDER : [activeTab];
  const visibleCount = visibleGroups.reduce((n, g) => n + (grouped.get(g)?.length ?? 0), 0);

  if (!open) return null;

  const run = (action: CommandAction) => {
    onClose();
    queueMicrotask(() => action.perform());
  };

  // Tab / Shift+Tab cycle the scope tabs (JetBrains mirror). preventDefault so
  // the key does not move focus out of the input or type into it.
  const cycleTab = (dir: 1 | -1) => {
    if (tabs.length < 3) return;
    const i = tabs.indexOf(activeTab);
    const next = tabs[(i + dir + tabs.length) % tabs.length];
    if (next) setActiveTab(next);
  };

  const onKeyDown = (e: ReactKeyboardEvent) => {
    if (e.key !== "Tab") return;
    e.preventDefault();
    cycleTab(e.shiftKey ? -1 : 1);
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      className="fixed inset-0 z-[60] flex items-start justify-center bg-black/80 backdrop-blur-sm animate-fade-in pt-[15vh] px-3"
      onClick={onClose}
      onKeyDown={onKeyDown}
      data-testid="command-palette-backdrop"
    >
      <Command
        label="Command palette"
        loop
        filter={(value, searchText) => scoreValue(value, searchText)}
        className="w-full max-w-[600px] bg-surface-800 border border-surface-700/50 rounded-lg shadow-2xl overflow-hidden animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2 px-4 h-12 border-b border-surface-700/50">
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeLinejoin="round"
            className="text-text-muted shrink-0"
          >
            <circle cx="11" cy="11" r="7" />
            <line x1="21" y1="21" x2="16.65" y2="16.65" />
          </svg>
          <Command.Input
            ref={inputRef}
            value={search}
            onValueChange={handleSearchChange}
            placeholder="Search actions, sessions, settings…"
            className="flex-1 bg-transparent outline-none text-[15px] text-text-primary placeholder:text-text-muted"
          />
          <kbd className="font-mono text-[10px] px-1.5 py-0.5 rounded bg-surface-900 border border-surface-700 text-text-muted">
            esc
          </kbd>
        </div>

        {tabs.length > 2 && (
          <div
            role="tablist"
            aria-label="Result categories"
            className="flex items-center gap-1 px-2 h-9 border-b border-surface-700/50"
          >
            {tabs.map((tab) => (
              <button
                key={tab}
                type="button"
                role="tab"
                aria-selected={activeTab === tab}
                onClick={() => setActiveTab(tab)}
                className={`px-2.5 py-1 rounded text-xs font-medium ${
                  activeTab === tab
                    ? "bg-surface-700 text-text-bright"
                    : "text-text-muted hover:text-text-primary hover:bg-surface-700/50"
                }`}
              >
                {tab}
              </button>
            ))}
            <span className="flex-1" />
            <kbd className="font-mono text-[10px] px-1.5 py-0.5 rounded bg-surface-900 border border-surface-700 text-text-muted">
              tab
            </kbd>
          </div>
        )}

        <Command.List className="max-h-[50vh] overflow-y-auto p-1">
          <Command.Empty className="px-4 py-8 text-center text-sm text-text-muted">No matches</Command.Empty>

          {visibleGroups.map((groupName) => {
            const items = grouped.get(groupName) ?? [];
            // The Conversations group still renders while a content search
            // is in flight, so the spinner replaces a premature "No matches".
            const showSpinner = groupName === "Conversations" && !!searching;
            if (items.length === 0 && !showSpinner) return null;
            return (
              <Command.Group
                key={groupName}
                heading={groupName}
                className="mb-1 [&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:pt-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-mono [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-wider [&_[cmdk-group-heading]]:text-text-muted"
              >
                {showSpinner && (
                  <Command.Item
                    value="conversation:__loading__"
                    disabled
                    className="flex items-center gap-2 px-3 h-9 rounded-md text-sm text-text-muted"
                  >
                    <span className="h-3 w-3 shrink-0 animate-spin rounded-full border-2 border-text-muted border-t-transparent" />
                    <span>Searching conversations…</span>
                  </Command.Item>
                )}
                {items.map((action) => {
                  return (
                    <Command.Item
                      key={action.id}
                      value={actionValue(action)}
                      onSelect={() => run(action)}
                      className="flex items-center gap-2 px-3 h-9 rounded-md cursor-pointer text-sm text-text-primary data-[selected=true]:bg-surface-700 data-[selected=true]:text-text-bright"
                    >
                      {action.status && (
                        <span className="font-mono text-text-muted w-4 shrink-0 text-center">
                          <StatusGlyph status={action.status} createdAt={action.statusCreatedAt ?? null} />
                        </span>
                      )}
                      {action.icon && <span className="shrink-0 text-text-muted">{action.icon}</span>}
                      <span className="truncate">{action.title}</span>
                      {action.subtitle && <span className="truncate text-text-muted text-xs">{action.subtitle}</span>}
                      <span className="flex-1" />
                      {action.shortcut && (
                        <kbd className="font-mono text-[10px] px-1.5 py-0.5 rounded bg-surface-900 border border-surface-700 text-text-muted">
                          {action.shortcut}
                        </kbd>
                      )}
                    </Command.Item>
                  );
                })}
              </Command.Group>
            );
          })}
        </Command.List>

        <div className="flex items-center justify-between px-4 h-8 border-t border-surface-700/50 text-[11px] font-mono text-text-muted">
          <span>↑↓ navigate · ↵ select · esc close</span>
          <span>
            {visibleCount} action{visibleCount === 1 ? "" : "s"}
          </span>
        </div>
      </Command>
      {cheat && <CheatOverlay key={cheat.id} effect={cheat.effect} onDone={clearCheat} />}
    </div>
  );
}
