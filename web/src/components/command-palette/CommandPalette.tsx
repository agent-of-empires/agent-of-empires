import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Command, defaultFilter } from "cmdk";
import { StatusGlyph } from "../StatusGlyph";
import { CheatOverlay } from "./CheatOverlay";
import { GROUP_ORDER } from "./groups";
import type { CommandAction, CommandActionGroup } from "./types";
import { matchCheat, type CheatEffect } from "../../lib/cheats";
import { reportInfo } from "../../lib/toastBus";

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
    }
  }

  // Stable so the overlay's cleanup timer is not reset by unrelated re-renders
  // (e.g. the user typing again while an effect is still on screen).
  const clearCheat = useCallback(() => setCheat(null), []);

  const grouped = useMemo(() => {
    const map = new Map<CommandActionGroup, CommandAction[]>();
    for (const g of GROUP_ORDER) map.set(g, []);
    for (const a of actions) {
      const arr = map.get(a.group);
      if (arr) arr.push(a);
    }
    return map;
  }, [actions]);

  if (!open) return null;

  const run = (action: CommandAction) => {
    onClose();
    queueMicrotask(() => action.perform());
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      className="fixed inset-0 z-[60] flex items-start justify-center bg-black/80 backdrop-blur-sm animate-fade-in pt-[15vh] px-3"
      onClick={onClose}
      data-testid="command-palette-backdrop"
    >
      <Command
        label="Command palette"
        loop
        filter={(value, searchText, keywords) =>
          // Conversation hits are already filtered server-side by content
          // (which the client text does not contain), so force-keep them;
          // everything else uses cmdk's default fuzzy scoring.
          value.startsWith("conversation:") ? 1 : defaultFilter!(value, searchText, keywords)
        }
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

        <Command.List className="max-h-[50vh] overflow-y-auto p-1">
          <Command.Empty className="px-4 py-8 text-center text-sm text-text-muted">No matches</Command.Empty>

          {GROUP_ORDER.map((groupName) => {
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
                  const searchValue = [action.title, action.subtitle ?? "", ...(action.keywords ?? [])].join(" ");
                  return (
                    <Command.Item
                      key={action.id}
                      value={`${action.id} ${searchValue}`}
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
            {actions.length} action{actions.length === 1 ? "" : "s"}
          </span>
        </div>
      </Command>
      {cheat && <CheatOverlay key={cheat.id} effect={cheat.effect} onDone={clearCheat} />}
    </div>
  );
}
