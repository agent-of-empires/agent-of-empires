import { useEffect } from "react";
import type { RightPanelView } from "../lib/rightPanelView";
import type { PluginPane } from "../lib/pluginPanes";

interface Entry {
  view: RightPanelView;
  label: string;
  hint: string;
}

// Mobile-only pseudo-views with no desktop dock equivalent; always offered.
const ALWAYS_ENTRIES: Entry[] = [
  { view: "agent", label: "Agent terminal", hint: "The session's main view" },
  { view: "paired", label: "Paired terminal", hint: "Host or container shell" },
];
// Mirrors the desktop `BuiltinPaneId`s that also have a mobile view (every
// one except "terminal", which is desktop's multi-instance extra-terminal
// dock and has no single-pane mobile equivalent). Gated by `availablePanes`.
const GATED_ENTRIES: Entry[] = [
  { view: "agents", label: "Sub agents", hint: "Background async sub-agents" },
  { view: "diff", label: "Diff", hint: "Changed files and review" },
  { view: "files", label: "Files", hint: "Browse the repo tree" },
];

interface Props {
  open: boolean;
  active: RightPanelView;
  pluginPanes: PluginPane[];
  // Builtin pane ids (and plugin ids) currently available: `allPaneIds` in
  // `App.tsx` filtered for mobile. Drives which of `GATED_ENTRIES` and which
  // plugin panes show up here, so mobile availability is derived from the
  // same capability/session gating as the desktop dock instead of a second,
  // independently maintained list.
  availablePanes: string[];
  onSelect: (view: RightPanelView) => void;
  onClose: () => void;
}

/** Mobile-only bottom sheet that promotes the chosen view into the single full-viewport main pane. */
export function MobileRightPanelPicker({ open, active, pluginPanes, availablePanes, onSelect, onClose }: Props) {
  // Close on Escape, matching the other dismissible overlays.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;
  return (
    <div className="md:hidden fixed inset-0 z-50 flex flex-col justify-end">
      <div
        className="absolute inset-0 bg-black/50"
        onClick={onClose}
        data-testid="mobile-right-panel-picker-backdrop"
      />
      <div
        className="relative bg-surface-900 border-t border-surface-700/20 rounded-t-lg pb-[env(safe-area-inset-bottom)]"
        role="dialog"
        aria-modal="true"
        aria-label="Select view"
        data-testid="mobile-right-panel-picker"
      >
        <div className="flex justify-center py-2">
          <div className="w-9 h-1 rounded-full bg-surface-500/40" />
        </div>
        <ul className="px-2 pb-2">
          {[...ALWAYS_ENTRIES, ...GATED_ENTRIES.filter((entry) => availablePanes.includes(entry.view))].map((entry) => {
            const isActive = entry.view === active;
            return (
              <li key={entry.view}>
                <button
                  onClick={() => onSelect(entry.view)}
                  aria-current={isActive ? "true" : undefined}
                  data-testid={`mobile-right-panel-pick-${entry.view}`}
                  className={`w-full flex flex-col items-start gap-0.5 px-3 py-2 rounded-lg text-left cursor-pointer transition-colors ${
                    isActive ? "bg-brand-600/10 text-brand-500" : "text-text-secondary hover:bg-surface-800"
                  }`}
                >
                  <span className="text-sm font-medium">{entry.label}</span>
                  <span className="text-xs text-text-dim">{entry.hint}</span>
                </button>
              </li>
            );
          })}
          {pluginPanes
            .filter((pane) => availablePanes.includes(pane.id))
            .map((pane) => {
              const isActive = pane.id === active;
              return (
                <li key={pane.id}>
                  <button
                    onClick={() => onSelect(pane.id as RightPanelView)}
                    aria-current={isActive ? "true" : undefined}
                    data-testid={`mobile-right-panel-pick-${pane.id}`}
                    className={`w-full flex flex-col items-start gap-0.5 px-3 py-2 rounded-lg text-left cursor-pointer transition-colors ${
                      isActive ? "bg-brand-600/10 text-brand-500" : "text-text-secondary hover:bg-surface-800"
                    }`}
                  >
                    <span className="text-sm font-medium">{pane.title}</span>
                    <span className="text-xs text-text-dim">Plugin</span>
                  </button>
                </li>
              );
            })}
        </ul>
      </div>
    </div>
  );
}
