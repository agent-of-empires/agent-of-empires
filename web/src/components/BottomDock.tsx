import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import { DockGroups, type DockGroupView } from "./DockGroups";
import type { DockControls, PaneDisplay } from "./Dock";
import type { DockLocation } from "../lib/panes";

const HEIGHT_KEY = "aoe-bottom-dock-height";
// Unsized, the dock takes a third of the window: room for a shell, as in an editor's terminal panel.
const DEFAULT_SHARE = 1 / 3;
const MIN_HEIGHT = 120;
// Leave at least this much for the main view above the dock while dragging.
const MIN_TOP_PX = 160;

function loadHeight(): number {
  const saved = safeGetItem(HEIGHT_KEY);
  if (saved) {
    const h = parseInt(saved, 10);
    if (h >= MIN_HEIGHT) return h;
  }
  return Math.max(MIN_HEIGHT, Math.round(window.innerHeight * DEFAULT_SHARE));
}

interface Props extends DockControls {
  groups: DockGroupView[];
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onMove: (id: string, dock: DockLocation) => void;
}

/** Full-width bottom dock: a height-resizable strip below the main+right-dock row. */
export function BottomDock({ groups, descriptorFor, renderBody, onActivate, onClose, onMove, ...controls }: Props) {
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
    <div
      ref={ref}
      data-region="dock-bottom"
      className="hidden md:flex flex-col shrink-0 border-t border-surface-700/60"
      style={{ height }}
    >
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
        {...controls}
      />
    </div>
  );
}
