import { useEffect, type ReactNode } from "react";
import { Sparkles, SquareTerminal, type LucideIcon } from "lucide-react";
import type { RightPanelView } from "../lib/rightPanelView";
import { sessionRowChromeClass } from "../lib/sessionRowChrome";
import type { PaneDisplay } from "./Dock";
import { PaneIcon } from "./PaneIcon";

// Mobile-only pseudo-views with no desktop dock equivalent; always offered.
const SESSION_VIEWS: { view: RightPanelView; title: string; icon: LucideIcon }[] = [
  { view: "agent", title: "Agent", icon: Sparkles },
  { view: "paired", title: "Paired terminal", icon: SquareTerminal },
];

interface Props {
  open: boolean;
  active: RightPanelView;
  sessionTitle: string;
  // `allPaneIds` in `App.tsx` filtered for mobile, so mobile availability
  // follows the desktop dock's capability and session gating.
  availablePanes: string[];
  describePane: (id: string) => PaneDisplay;
  onSelect: (view: RightPanelView) => void;
  onClose: () => void;
}

function Section({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="pt-2">
      <div className="px-3 pb-1 text-[11px] font-mono uppercase tracking-wider text-text-dim">{label}</div>
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

  // Stays mounted so it can slide out; `invisible` flips after the transform
  // finishes, which also takes the closed drawer out of focus and the a11y tree.
  return (
    <div className="md:hidden">
      <div
        className={`fixed top-12 inset-x-0 bottom-0 z-40 bg-black/50 transition-[opacity,visibility] duration-300 motion-reduce:transition-none ${
          open ? "opacity-100" : "opacity-0 invisible"
        }`}
        onClick={onClose}
        data-testid="mobile-right-panel-picker-backdrop"
      />
      <div
        className={`fixed top-12 right-0 bottom-0 z-50 w-[280px] max-w-[85vw] bg-surface-800 border-l border-surface-700/60 flex flex-col pr-[env(safe-area-inset-right)] pb-[env(safe-area-inset-bottom)] transition-[translate,visibility] duration-300 ease-in-out motion-reduce:transition-none ${
          open ? "translate-x-0" : "translate-x-full invisible"
        }`}
        role="dialog"
        aria-modal="true"
        aria-label="Panels"
        data-testid="mobile-right-panel-picker"
      >
        <div className="px-3 pt-3 pb-2 border-b border-surface-700/60">
          <div className="text-sm text-text-muted">Panels</div>
          <div className="mt-0.5 truncate font-mono text-[12px] text-text-dim">{sessionTitle}</div>
        </div>
        <div className="flex-1 min-h-0 overflow-y-auto flex flex-col pb-2">
          <div className="mt-auto" />
          <Section label="Session">
            {SESSION_VIEWS.map(({ view, title, icon }) => (
              <Row key={view} display={{ title, icon }} view={view} active={view === active} onSelect={onSelect} />
            ))}
          </Section>
          {availablePanes.length > 0 && (
            <Section label="Panes">
              {availablePanes.map((id) => (
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
        </div>
      </div>
    </div>
  );
}
