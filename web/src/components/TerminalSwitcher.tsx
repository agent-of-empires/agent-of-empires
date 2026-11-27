import { useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { createPortal } from "react-dom";
import { ChevronDown, Plus, SquareTerminal } from "lucide-react";

import { shellLabel, type PairedShell, type PairedShellGroup } from "../lib/pairedShells";
import { DropdownMenu, MenuItem, MenuSeparator } from "./ContextMenu";
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
  return (
    <DropdownMenu
      initialFocus="first"
      label="Paired terminals"
      testId="terminal-switcher-menu"
      style={maxHeight === undefined ? undefined : { maxHeight }}
      className="w-72 max-h-[60vh]"
    >
      {groups.length === 0 && <div className="px-3 py-1.5 text-[13px] text-text-dim">No paired terminals are open</div>}
      {groups.map(({ session, shells }) => (
        <div key={session.id} role="group" aria-label={session.title} className="pb-1">
          <div className="px-3 pt-2 pb-1 truncate font-mono text-[11px] text-text-dim" title={session.project_path}>
            {session.title}
          </div>
          {shells.map((shell) => (
            <MenuItem
              key={`${shell.index}:${shell.mode}`}
              role="menuitem"
              testId={`terminal-switcher-item-${shell.sessionId}-${shell.index}-${shell.mode}`}
              icon={<SquareTerminal className="text-text-dim" />}
              trailing={
                (session.is_sandboxed || shell.open) &&
                [session.is_sandboxed && (shell.mode === "container" ? "Container" : "Host"), shell.open && "open"]
                  .filter(Boolean)
                  .join(" · ")
              }
              onClick={() => {
                onDone();
                onPick(shell);
              }}
            >
              {shellLabel(shell.index)}
            </MenuItem>
          ))}
        </div>
      ))}
      {onNew && newTerminalFor !== undefined && (
        <>
          <MenuSeparator />
          <MenuItem
            role="menuitem"
            testId="terminal-switcher-new"
            icon={<Plus className="text-text-dim" />}
            onClick={() => {
              onDone();
              onNew();
            }}
          >
            New terminal for {newTerminalFor}
          </MenuItem>
        </>
      )}
    </DropdownMenu>
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
          <div ref={menuRef} style={placement.position} className="z-50">
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
      <div ref={ref} className="pointer-events-auto">
        <TerminalSwitcherMenu {...props} onDone={onClose} />
      </div>
    </div>
  );
}
