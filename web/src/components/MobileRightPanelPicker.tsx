import { useEffect, type ReactNode } from "react";
import { SquareTerminal } from "lucide-react";
import type { PairedShell, PairedShellGroup } from "../lib/pairedShells";
import type { RightPanelView } from "../lib/rightPanelView";
import { sessionRowChromeClass } from "../lib/sessionRowChrome";
import { BACKDROP, SHEET, SHEET_HANDLE } from "../lib/surfaces";
import type { PaneDisplay } from "./Dock";
import { PaneIcon } from "./PaneIcon";

/** Views the phone's ⋯ menu lists itself, so More panes leaves them out. */
const TAB_VIEWS = new Set<string>(["agent", "paired", "diff", "files"]);

/** The sheet moves on the iOS sheet curve; reduced motion skips it. */
const SHEET_MOTION = "duration-[400ms] ease-[cubic-bezier(0.32,0.72,0,1)] motion-reduce:transition-none";

interface Props {
  open: boolean;
  active: RightPanelView;
  sessionTitle: string;
  // `allPaneIds` in `App.tsx` filtered for mobile, so mobile availability
  // follows the desktop dock's capability and session gating.
  availablePanes: string[];
  describePane: (id: string) => PaneDisplay;
  /** Every live paired shell, so the paired view can show another Session's. */
  shellGroups: PairedShellGroup[];
  onPickShell: (shell: PairedShell) => void;
  /** The shell the paired view shows, while it is the current view. */
  shownShell: Pick<PairedShell, "sessionId" | "index" | "mode"> | null;
  onSelect: (view: RightPanelView) => void;
  onClose: () => void;
}

function Section({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="pt-2">
      <div className="px-3 pb-1 text-[12px] font-medium text-text-dim">{label}</div>
      <ul>{children}</ul>
    </div>
  );
}

function Row({
  display,
  view,
  active,
  onSelect,
}: {
  display: PaneDisplay;
  view: RightPanelView;
  active: boolean;
  onSelect: (view: RightPanelView) => void;
}) {
  return (
    <li>
      <button
        onClick={() => onSelect(view)}
        aria-current={active ? "true" : undefined}
        data-testid={`mobile-right-panel-pick-${view}`}
        className={`w-full h-11 flex items-center gap-3 px-3 text-left text-[14px] cursor-pointer transition-colors duration-75 focus:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-600 ${
          active ? "text-text-primary" : "text-text-secondary"
        } ${sessionRowChromeClass(active, false)}`}
      >
        <PaneIcon
          icon={display.icon}
          iconAssetUrl={display.iconAssetUrl}
          className={`h-4 w-4 shrink-0 ${active ? "text-brand-500" : "text-text-dim"}`}
        />
        <span className="truncate">{display.title}</span>
      </button>
    </li>
  );
}

/** Mobile-only right drawer, the workspace sidebar's counterpart, that
 *  promotes the chosen view into the single full-viewport main pane. Rows sit
 *  at the bottom, within thumb reach. */
export function MobileRightPanelPicker({
  open,
  active,
  sessionTitle,
  availablePanes,
  describePane,
  shellGroups,
  onPickShell,
  shownShell,
  onSelect,
  onClose,
}: Props) {
  // Close on Escape, matching the other dismissible overlays.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  const panes = availablePanes.filter((id) => !TAB_VIEWS.has(id));

  // The ⋯ menu's More panes: a bottom sheet with what the menu leaves out. Stays mounted so it can slide out;
  // `invisible` flips after the transform finishes, which also takes the closed sheet out of focus and the a11y tree.
  return (
    <div className="md:hidden">
      <div
        className={`fixed inset-0 z-40 ${BACKDROP} transition-[opacity,visibility] ${SHEET_MOTION} ${
          open ? "opacity-100" : "opacity-0 invisible"
        }`}
        onClick={onClose}
        data-testid="mobile-right-panel-picker-backdrop"
      />
      <div
        className={`fixed inset-x-0 bottom-0 z-50 max-h-[75dvh] ${SHEET} flex flex-col px-[env(safe-area-inset-left)] pb-[env(safe-area-inset-bottom)] transition-[translate,visibility] ${SHEET_MOTION} ${
          open ? "translate-y-0" : "translate-y-full invisible"
        }`}
        role="dialog"
        aria-modal="true"
        aria-label="More"
        data-testid="mobile-right-panel-picker"
      >
        <div className={SHEET_HANDLE} aria-hidden />
        <div className="px-4 pt-2 pb-2 flex items-baseline gap-2">
          <div className="text-[15px] font-medium text-text-primary">More</div>
          <div className="min-w-0 truncate text-[12px] text-text-dim">{sessionTitle}</div>
        </div>
        <div className="flex-1 min-h-0 overflow-y-auto flex flex-col pb-2">
          {panes.length > 0 && (
            <Section label="Panes">
              {panes.map((id) => (
                <Row
                  key={id}
                  display={describePane(id)}
                  view={id as RightPanelView}
                  active={id === active}
                  onSelect={onSelect}
                />
              ))}
            </Section>
          )}
          {shellGroups.length > 0 && (
            <Section label="Paired terminals">
              {shellGroups.flatMap(({ session, shells }) =>
                shells.map((shell) => {
                  const current =
                    shownShell?.sessionId === shell.sessionId &&
                    shownShell.index === shell.index &&
                    shownShell.mode === shell.mode;
                  return (
                    <li key={`${shell.sessionId}:${shell.index}:${shell.mode}`}>
                      <button
                        onClick={() => onPickShell(shell)}
                        aria-current={current ? "true" : undefined}
                        data-testid={`mobile-shell-pick-${shell.sessionId}-${shell.index}-${shell.mode}`}
                        className={`w-full h-11 flex items-center gap-3 px-3 text-left text-[14px] cursor-pointer transition-colors duration-75 focus:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-600 text-text-secondary ${sessionRowChromeClass(current, false)}`}
                      >
                        <SquareTerminal className="h-4 w-4 shrink-0 text-text-dim" />
                        <span className="truncate">
                          {session.title} · {shell.index + 1}
                        </span>
                        {session.is_sandboxed && (
                          <span className="ml-auto shrink-0 font-mono text-[11px] text-text-dim">
                            {shell.mode === "container" ? "Container" : "Host"}
                          </span>
                        )}
                      </button>
                    </li>
                  );
                }),
              )}
            </Section>
          )}
        </div>
      </div>
    </div>
  );
}
