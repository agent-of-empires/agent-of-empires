import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { createPortal } from "react-dom";
import { ChevronDown, Plus, SquareTerminal } from "lucide-react";

import { shellLabel, type PairedShell, type PairedShellGroup } from "../lib/pairedShells";
import { moveMenuFocus } from "./menuFocus";
import { useOutsideDismiss } from "./useOutsideDismiss";

export interface TerminalSwitcherProps {
  groups: PairedShellGroup[];
  /** The selected Session's title, for "New terminal for …"; omitted where no terminal can be created. */
  newTerminalFor?: string;
  onPick: (shell: PairedShell) => void;
  onNew?: () => void;
}

/** Every live paired shell grouped by Session; picking one shows it in a terminal tab. Focuses its first item. */
export function TerminalSwitcherMenu({
  groups,
  newTerminalFor,
  onPick,
  onNew,
  onDone,
  maxHeight,
}: TerminalSwitcherProps & { onDone: () => void; maxHeight?: number }) {
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
      onKeyDown={moveMenuFocus}
      style={maxHeight === undefined ? undefined : { maxHeight }}
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

const MENU_GAP = 4;
const VIEWPORT_MARGIN = 8;

/** Where the menu fits beside `trigger`: below it unless more room is above, capped to that room. */
function menuPlacement(trigger: DOMRect): { position: CSSProperties; maxHeight: number } {
  const below = window.innerHeight - trigger.bottom - MENU_GAP - VIEWPORT_MARGIN;
  const above = trigger.top - MENU_GAP - VIEWPORT_MARGIN;
  const up = above > below;
  const right = Math.max(VIEWPORT_MARGIN, window.innerWidth - trigger.right);
  return {
    position: up
      ? { position: "fixed", right, bottom: window.innerHeight - trigger.top + MENU_GAP }
      : { position: "fixed", right, top: trigger.bottom + MENU_GAP },
    maxHeight: Math.max(0, up ? above : below),
  };
}

/** The dock strip's switcher trigger. The menu renders in a portal, so the dock's overflow never clips it. */
export function TerminalSwitcherButton(props: TerminalSwitcherProps) {
  const [open, setOpen] = useState(false);
  const [placement, setPlacement] = useState<ReturnType<typeof menuPlacement> | null>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    setPlacement(open && triggerRef.current ? menuPlacement(triggerRef.current.getBoundingClientRect()) : null);
  }, [open]);
  useOutsideDismiss(open, [triggerRef, menuRef], (e) => {
    setOpen(false);
    // Escape takes focus from the menu with it; a click elsewhere already moved it.
    if (e.type === "keydown") triggerRef.current?.focus();
  });
  return (
    <>
      <button
        ref={triggerRef}
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
      {open &&
        placement &&
        createPortal(
          <div ref={menuRef} style={placement.position} className="z-50 animate-fade-in">
            <TerminalSwitcherMenu {...props} maxHeight={placement.maxHeight} onDone={() => setOpen(false)} />
          </div>,
          document.body,
        )}
    </>
  );
}

/** The switcher opened from the keyboard, above the dashboard. */
export function TerminalSwitcherOverlay({ onClose, ...props }: TerminalSwitcherProps & { onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const [returnTo] = useState(() => (document.activeElement instanceof HTMLElement ? document.activeElement : null));
  useOutsideDismiss(true, [ref], (e) => {
    onClose();
    if (e.type === "keydown" && returnTo?.isConnected) returnTo.focus();
  });
  return (
    <div className="fixed inset-x-0 top-[15vh] z-50 flex justify-center px-4 pointer-events-none">
      <div ref={ref} className="pointer-events-auto animate-fade-in">
        <TerminalSwitcherMenu {...props} onDone={onClose} />
      </div>
    </div>
  );
}
