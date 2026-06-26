import { useCallback, useEffect, useState } from "react";

import { safeGetItem, safeSetItem } from "./safeStorage";
import type { BuiltinPaneId } from "./panes";

const LAYOUT_KEY = "aoe-pane-layout";
// The pre-pane single right-column collapse flag (#2405 and earlier). Read once
// to seed the new per-pane state so an existing user keeps their open/collapsed
// choice across the upgrade, then superseded by LAYOUT_KEY.
const LEGACY_COLLAPSED_KEY = "aoe-right-collapsed";

/** Which built-in panes are currently open. Each pane toggles independently;
 *  the right dock renders whichever are open (both open reproduces the old
 *  diff-over-terminal stack). */
export type PaneLayout = Record<BuiltinPaneId, boolean>;

function defaults(): PaneLayout {
  // Desktop opens both panes (matches the historical expanded right column);
  // narrow viewports start collapsed and drive the surface via the mobile
  // picker instead.
  const open = window.innerWidth >= 768;
  return { diff: open, terminal: open };
}

function load(): PaneLayout {
  const raw = safeGetItem(LAYOUT_KEY);
  if (raw) {
    try {
      const p = JSON.parse(raw) as Partial<Record<BuiltinPaneId, unknown>>;
      const base = defaults();
      return {
        diff: typeof p.diff === "boolean" ? p.diff : base.diff,
        terminal: typeof p.terminal === "boolean" ? p.terminal : base.terminal,
      };
    } catch {
      // Malformed JSON: fall through to legacy migration / defaults.
    }
  }
  const legacy = safeGetItem(LEGACY_COLLAPSED_KEY);
  if (legacy === "1") return { diff: false, terminal: false };
  if (legacy === "0") return { diff: true, terminal: true };
  return defaults();
}

export interface PaneLayoutApi {
  layout: PaneLayout;
  togglePane: (id: BuiltinPaneId) => void;
  setPaneOpen: (id: BuiltinPaneId, open: boolean) => void;
}

export function usePaneLayout(): PaneLayoutApi {
  const [layout, setLayout] = useState(load);
  useEffect(() => {
    safeSetItem(LAYOUT_KEY, JSON.stringify(layout));
  }, [layout]);
  const togglePane = useCallback((id: BuiltinPaneId) => setLayout((l) => ({ ...l, [id]: !l[id] })), []);
  const setPaneOpen = useCallback(
    (id: BuiltinPaneId, open: boolean) => setLayout((l) => (l[id] === open ? l : { ...l, [id]: open })),
    [],
  );
  return { layout, togglePane, setPaneOpen };
}
