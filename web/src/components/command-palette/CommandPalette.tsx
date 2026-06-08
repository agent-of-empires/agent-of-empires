import { useEffect, useMemo, useRef } from "react";
import { Command } from "cmdk";
import { StatusGlyph } from "../StatusGlyph";
import { GROUP_ORDER } from "./groups";
import type { CommandAction, CommandActionGroup } from "./types";

interface Props {
  open: boolean;
  onClose: () => void;
  actions: CommandAction[];
}

export function CommandPalette({ open, onClose, actions }: Props) {
  const inputRef = useRef<HTMLInputElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

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
            placeholder="Search actions, sessions, settings…"
            className="flex-1 bg-transparent outline-none text-[15px] text-text-primary placeholder:text-text-muted"
          />
          <kbd className="font-mono text-[10px] px-1.5 py-0.5 rounded bg-surface-900 border border-surface-700 text-text-muted">
            esc
          </kbd>
        </div>

        <Command.List className="max-h-[50vh] overflow-y-auto p-1">
          <Command.Empty className="px-4 py-8 text-center text-sm text-text-muted">
            No matches
          </Command.Empty>

          {GROUP_ORDER.map((groupName) => {
            const items = grouped.get(groupName) ?? [];
            if (items.length === 0) return null;
            return (
              <Command.Group
                key={groupName}
                heading={groupName}
                className="mb-1 [&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:pt-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-mono [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-wider [&_[cmdk-group-heading]]:text-text-muted"
              >
                {items.map((action) => {
                  const searchValue = [
                    action.title,
                    action.subtitle ?? "",
                    ...(action.keywords ?? []),
                  ].join(" ");
                  return (
                    <Command.Item
                      key={action.id}
                      value={`${action.id} ${searchValue}`}
                      onSelect={() => run(action)}
                      className="flex items-center gap-2 px-3 h-9 rounded-md cursor-pointer text-sm text-text-primary data-[selected=true]:bg-surface-700 data-[selected=true]:text-text-bright"
                    >
                      {action.status && (
                        <span className="font-mono text-text-muted w-4 shrink-0 text-center">
                          <StatusGlyph
                            status={action.status}
                            createdAt={action.statusCreatedAt ?? null}
                          />
                        </span>
                      )}
                      {action.icon && (
                        <span className="shrink-0 text-text-muted">
                          {action.icon}
                        </span>
                      )}
                      <span className="truncate">{action.title}</span>
                      {action.subtitle && (
                        <span className="truncate text-text-muted text-xs">
                          {action.subtitle}
                        </span>
                      )}
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
    </div>
  );
}
