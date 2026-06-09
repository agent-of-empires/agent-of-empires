/* eslint-disable react-refresh/only-export-components */
// Transcript-level tool-card density. A purely client-side view
// preference (no daemon config, no settings parity): "detailed" keeps
// the existing per-card defaults; "compact" makes every tool card
// default collapsed so a long post-plan implementation stream stays
// scannable without touching the automatic grouping in
// AcpRuntime.tsx. See #1767.
//
// The override model lives in useToolCardExpansion (ToolCards.tsx):
// a user's per-card toggle is scoped to the active density, so flipping
// the toggle re-applies the baseline for every card without an effect.

import { createContext, useCallback, useContext, useState } from "react";
import { ListCollapse } from "lucide-react";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";

export type ToolDensity = "detailed" | "compact";

const STORAGE_KEY = "aoe.acp.toolDensity.v1";

function readStoredDensity(): ToolDensity {
  return safeGetItem(STORAGE_KEY) === "compact" ? "compact" : "detailed";
}

/** Client-side density preference, persisted in localStorage so it
 *  survives reloads. Defaults to "detailed" so existing users and
 *  snapshots are unaffected. */
export function useToolDensityPref(): [ToolDensity, () => void] {
  const [density, setDensity] = useState<ToolDensity>(readStoredDensity);
  const toggle = useCallback(() => {
    setDensity((prev) => {
      const next: ToolDensity = prev === "compact" ? "detailed" : "compact";
      safeSetItem(STORAGE_KEY, next);
      return next;
    });
  }, []);
  return [density, toggle];
}

const ToolDisplayModeContext = createContext<ToolDensity>("detailed");

export function ToolDisplayModeProvider({ density, children }: { density: ToolDensity; children: React.ReactNode }) {
  return <ToolDisplayModeContext.Provider value={density}>{children}</ToolDisplayModeContext.Provider>;
}

/** Active tool-card density. Defaults to "detailed" outside a provider
 *  so cards rendered in isolation (tests, storybook) keep their
 *  pre-existing expansion behaviour. */
export function useToolDisplayMode(): ToolDensity {
  return useContext(ToolDisplayModeContext);
}

/** Transcript control that flips tool-card density. `aria-pressed`
 *  carries the on/off state; the label stays constant so the control
 *  reads the same whether or not it is engaged. */
export function ToolDensityToggle({ density, onToggle }: { density: ToolDensity; onToggle: () => void }) {
  const compact = density === "compact";
  return (
    <button
      type="button"
      aria-pressed={compact}
      onClick={onToggle}
      title={
        compact
          ? "Tool cards collapsed for scanning; click to restore detail"
          : "Collapse every tool card to its header for scanning"
      }
      className={[
        "ml-auto inline-flex items-center gap-1.5 rounded-md border px-2 py-1",
        "text-[11px] uppercase tracking-wider transition-colors",
        compact
          ? "border-brand-700/50 bg-brand-700/10 text-brand-400"
          : "border-surface-700 bg-surface-800 text-text-dim hover:bg-surface-700",
      ].join(" ")}
    >
      <ListCollapse className="h-3.5 w-3.5" />
      Compact tools
    </button>
  );
}
