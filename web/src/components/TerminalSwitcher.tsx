import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { ChevronDown, Plus, SquareTerminal } from "lucide-react";

import { shellLabel, type PairedShell, type PairedShellGroup } from "../lib/pairedShells";
import { useOutsideDismiss } from "./useOutsideDismiss";

export interface TerminalSwitcherProps {
  groups: PairedShellGroup[];
  /** The selected Session's title, for "New terminal for …"; omitted where no terminal can be created. */
  newTerminalFor?: string;
  onPick: (shell: PairedShell) => void;
  onNew?: () => void;
}

function moveFocus(e: KeyboardEvent<HTMLElement>) {
  const items = [...e.currentTarget.querySelectorAll<HTMLElement>('[role="menuitem"]')];
  const at = items.indexOf(document.activeElement as HTMLElement);
  const next =
    e.key === "ArrowDown"
      ? (at + 1) % items.length
      : e.key === "ArrowUp"
        ? (at - 1 + items.length) % items.length
        : e.key === "Home"
          ? 0
          : e.key === "End"
            ? items.length - 1
            : null;
  if (next === null || items.length === 0) return;
  e.preventDefault();
  items[next]!.focus();
}

/** Every live paired shell grouped by Session; picking one shows it in a terminal tab. Focuses its first item. */
export function TerminalSwitcherMenu({
  groups,
  newTerminalFor,
  onPick,
  onNew,
  onDone,
}: TerminalSwitcherProps & { onDone: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.querySelector<HTMLElement>('[role="menuitem"]')?.focus();
  }, []);
  const item =
    "w-full flex items-center gap-2 h-8 px-3 text-left text-[12px] text-text-secondary cursor-pointer hover:bg-surface-700/60 hover:text-text-primary focus:outline-none focus-visible:bg-surface-700/60";
  return (
    <div
      ref={ref}
      role="menu"
      aria-label="Paired terminals"
      data-testid="terminal-switcher-menu"
      data-owns-escape="open"
      onKeyDown={moveFocus}
      className="w-72 max-h-[60vh] overflow-y-auto bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1"
    >
      {groups.length === 0 && <div className="px-3 py-1.5 text-[12px] text-text-dim">No paired terminals are open</div>}
      {groups.map(({ session, shells }) => (
        <div key={session.id} role="group" aria-label={session.title} className="pb-1">
          <div className="px-3 pt-1.5 pb-0.5 truncate font-mono text-[11px] text-text-dim" title={session.project_path}>
            {session.title}
          </div>
          {shells.map((shell) => (
            <button
              key={`${shell.index}:${shell.mode}`}
              type="button"
              role="menuitem"
              data-testid={`terminal-switcher-item-${shell.sessionId}-${shell.index}-${shell.mode}`}
              onClick={() => {
                onDone();
                onPick(shell);
              }}
              className={item}
            >
              <SquareTerminal className="size-3.5 shrink-0 text-text-dim" aria-hidden />
              <span className="truncate">{shellLabel(shell.index)}</span>
              {session.is_sandboxed && (
                <span className="font-mono text-[11px] text-text-dim">
                  {shell.mode === "container" ? "Container" : "Host"}
                </span>
              )}
              {shell.open && <span className="ml-auto font-mono text-[11px] text-text-dim">open</span>}
            </button>
          ))}
        </div>
      ))}
      {onNew && newTerminalFor !== undefined && (
        <>
          <div className="my-1 border-t border-surface-700/50" role="separator" />
          <button
            type="button"
            role="menuitem"
            data-testid="terminal-switcher-new"
            onClick={() => {
              onDone();
              onNew();
            }}
            className={item}
          >
            <Plus className="size-3.5 shrink-0 text-text-dim" aria-hidden />
            <span className="truncate">New terminal for {newTerminalFor}</span>
          </button>
        </>
      )}
    </div>
  );
}

/** The dock strip's switcher trigger. */
export function TerminalSwitcherButton(props: TerminalSwitcherProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useOutsideDismiss(open, [ref], () => setOpen(false));
  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        title="Switch terminal"
        aria-label="Switch terminal"
        data-testid="terminal-switcher-toggle"
        className="w-5 h-5 flex items-center justify-center shrink-0 rounded text-text-dim hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
      >
        <ChevronDown className="size-3.5" aria-hidden />
      </button>
      {open && (
        <div className="absolute right-0 top-full mt-1 z-50 animate-fade-in">
          <TerminalSwitcherMenu {...props} onDone={() => setOpen(false)} />
        </div>
      )}
    </div>
  );
}

/** The switcher opened from the keyboard, above the dashboard. */
export function TerminalSwitcherOverlay({ onClose, ...props }: TerminalSwitcherProps & { onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useOutsideDismiss(true, [ref], onClose);
  return (
    <div className="fixed inset-x-0 top-[15vh] z-50 flex justify-center px-4 pointer-events-none">
      <div ref={ref} className="pointer-events-auto animate-fade-in">
        <TerminalSwitcherMenu {...props} onDone={onClose} />
      </div>
    </div>
  );
}
