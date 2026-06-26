import { useCallback, useEffect, useState } from "react";

import { safeGetItem, safeSetItem } from "./safeStorage";
import { BUILTIN_PANES, type BuiltinPaneId, type DockLocation } from "./panes";

const LAYOUT_KEY = "aoe-pane-layout";
// The pre-pane single right-column collapse flag (#2405 and earlier). Read once
// to seed the new per-pane state so an existing user keeps their open/collapsed
// choice across the upgrade, then superseded by LAYOUT_KEY.
const LEGACY_COLLAPSED_KEY = "aoe-right-collapsed";

export interface PaneState {
  open: boolean;
  dock: DockLocation;
}

/** Open state and dock location for each built-in pane. The right dock renders
 *  its open panes as a vertical split; the bottom dock as a horizontal one. */
export type PaneLayout = Record<BuiltinPaneId, PaneState>;

function isDock(v: unknown): v is DockLocation {
  return v === "right" || v === "bottom";
}

function defaults(): PaneLayout {
  // Desktop opens both panes in their default dock (matches the historical
  // expanded right column); narrow viewports start collapsed and drive the
  // surface via the mobile picker instead.
  const open = window.innerWidth >= 768;
  const out = {} as PaneLayout;
  for (const p of BUILTIN_PANES) out[p.id] = { open, dock: p.defaultDock };
  return out;
}

function seed(open: boolean): PaneLayout {
  const out = {} as PaneLayout;
  for (const p of BUILTIN_PANES) out[p.id] = { open, dock: p.defaultDock };
  return out;
}

function load(): PaneLayout {
  const raw = safeGetItem(LAYOUT_KEY);
  if (raw) {
    try {
      const p = JSON.parse(raw) as Record<string, unknown>;
      const base = defaults();
      for (const pane of BUILTIN_PANES) {
        const v = p[pane.id];
        if (typeof v === "boolean") {
          // Phase-1 shape: a bare open boolean, no dock yet.
          base[pane.id] = { open: v, dock: pane.defaultDock };
        } else if (v && typeof v === "object") {
          const s = v as Record<string, unknown>;
          base[pane.id] = {
            open: typeof s.open === "boolean" ? s.open : base[pane.id].open,
            dock: isDock(s.dock) ? s.dock : pane.defaultDock,
          };
        }
      }
      return base;
    } catch {
      // Malformed JSON: fall through to legacy migration / defaults.
    }
  }
  const legacy = safeGetItem(LEGACY_COLLAPSED_KEY);
  if (legacy === "1") return seed(false);
  if (legacy === "0") return seed(true);
  return defaults();
}

export interface PaneLayoutApi {
  layout: PaneLayout;
  togglePane: (id: BuiltinPaneId) => void;
  setPaneOpen: (id: BuiltinPaneId, open: boolean) => void;
  movePane: (id: BuiltinPaneId, dock: DockLocation) => void;
}

export function usePaneLayout(): PaneLayoutApi {
  const [layout, setLayout] = useState(load);
  useEffect(() => {
    safeSetItem(LAYOUT_KEY, JSON.stringify(layout));
  }, [layout]);
  const togglePane = useCallback(
    (id: BuiltinPaneId) => setLayout((l) => ({ ...l, [id]: { ...l[id], open: !l[id].open } })),
    [],
  );
  const setPaneOpen = useCallback(
    (id: BuiltinPaneId, open: boolean) =>
      setLayout((l) => (l[id].open === open ? l : { ...l, [id]: { ...l[id], open } })),
    [],
  );
  const movePane = useCallback(
    (id: BuiltinPaneId, dock: DockLocation) =>
      setLayout((l) => (l[id].dock === dock ? l : { ...l, [id]: { ...l[id], dock } })),
    [],
  );
  return { layout, togglePane, setPaneOpen, movePane };
}

/** Ids of open panes docked at `location`, in registry order. */
export function openPanesAt(layout: PaneLayout, location: DockLocation): BuiltinPaneId[] {
  return BUILTIN_PANES.filter((p) => layout[p.id].open && layout[p.id].dock === location).map((p) => p.id);
}
