import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import { DockGroups, type DockGroupView } from "./DockGroups";
import type { PaneDisplay } from "./Dock";
import type { DockLocation } from "../lib/panes";

const HEIGHT_KEY = "aoe-bottom-dock-height";
const DEFAULT_HEIGHT = 240;
const MIN_HEIGHT = 120;
// Leave at least this much for the main view above the dock while dragging.
const MIN_TOP_PX = 160;

function loadHeight(): number {
  const saved = safeGetItem(HEIGHT_KEY);
  if (saved) {
    const h = parseInt(saved, 10);
    if (h >= MIN_HEIGHT) return h;
  }
  return DEFAULT_HEIGHT;
}

interface Props {
  groups: DockGroupView[];
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onMove: (id: string, dock: DockLocation) => void;
  onNewTerminal?: () => void;
}

/** Full-width bottom dock: a height-resizable strip below the main+right-dock
 *  row. Hidden by the parent when it has no open panes. Holds one or more
 *  stacked groups. Desktop only; mobile uses the single full-viewport view
 *  picker. */
export function BottomDock({ groups, descriptorFor, renderBody, onActivate, onClose, onMove, onNewTerminal }: Props) {
  const [height, setHeight] = useState(loadHeight);
  const ref = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.cursor = "row-resize";
    document.body.style.userSelect = "none";
  }, []);

  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (!dragging.current || !ref.current) return;
      const floor = ref.current.getBoundingClientRect().bottom;
      const next = floor - e.clientY;
      if (next < MIN_HEIGHT || e.clientY < MIN_TOP_PX) return;
      setHeight(next);
    };
    const settle = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setHeight((h) => {
        safeSetItem(HEIGHT_KEY, String(h));
        return h;
      });
      window.dispatchEvent(new Event("resize"));
    };
    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", settle);
    return () => {
      document.removeEventListener("mousemove", onMouseMove);
      document.removeEventListener("mouseup", settle);
      if (dragging.current) {
        dragging.current = false;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      }
    };
  }, []);

  return (
    <div ref={ref} className="hidden md:flex flex-col shrink-0 border-t border-surface-700/60" style={{ height }}>
      <div
        data-testid="bottom-dock-resize"
        onMouseDown={handleMouseDown}
        className="h-1 cursor-row-resize shrink-0 hover:bg-brand-600/50 transition-colors duration-75"
      />
      <DockGroups
        location="bottom"
        groups={groups}
        descriptorFor={descriptorFor}
        renderBody={renderBody}
        onActivate={onActivate}
        onClose={onClose}
        onMove={onMove}
        onNewTerminal={onNewTerminal}
      />
    </div>
  );
}
