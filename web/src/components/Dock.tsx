import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import { PaneFrame } from "./PaneFrame";
import type { DockLocation } from "../lib/panes";

export interface PaneDisplay {
  title: string;
  icon: LucideIcon;
}

interface Props {
  location: DockLocation;
  paneIds: string[];
  /** Title + icon for a pane id (built-in from the registry, or a plugin pane).
   *  A callback rather than an array prop so the icon component is resolved
   *  inside the dock, keeping the parent's render free of element arrays. */
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onMove: (id: string, dock: DockLocation) => void;
  onClose: (id: string) => void;
}

const DEFAULT_RATIO = 0.5;
const MIN_PX = 80;

function ratioKey(location: DockLocation): string {
  return `aoe-dock-split-${location}`;
}

function loadRatio(location: DockLocation): number {
  const saved = safeGetItem(ratioKey(location));
  if (saved) {
    const r = parseFloat(saved);
    if (r > 0 && r < 1) return r;
  }
  return DEFAULT_RATIO;
}

/** Renders the open panes for one dock location: a vertical split for the
 *  right dock, a horizontal split for the bottom dock. The parent passes the
 *  open pane ids plus a `renderBody` callback; this looks up each pane's title
 *  and icon from the registry and wraps the body in tool-window chrome. The
 *  parent hides the dock entirely when it has no open panes.
 *
 *  ponytail: a single draggable divider between two panes. Two built-in panes
 *  (diff, terminal) means a dock holds at most two today; a third would render
 *  at an equal flex share with no handle until this grows a multi-divider model. */
export function Dock({ location, paneIds, descriptorFor, renderBody, onMove, onClose }: Props) {
  const vertical = location === "right";
  const [ratio, setRatio] = useState(() => loadRatio(location));
  const containerRef = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);

  const handleMouseDown = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      dragging.current = true;
      document.body.style.cursor = vertical ? "row-resize" : "col-resize";
      document.body.style.userSelect = "none";
    },
    [vertical],
  );

  useEffect(() => {
    const apply = (client: number) => {
      const el = containerRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      const span = vertical ? rect.height : rect.width;
      const pos = client - (vertical ? rect.top : rect.left);
      if (pos < MIN_PX || span - pos < MIN_PX) return;
      setRatio(pos / span);
    };
    const settle = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setRatio((r) => {
        safeSetItem(ratioKey(location), String(r));
        return r;
      });
      window.dispatchEvent(new Event("resize"));
    };
    const onMouseMove = (e: MouseEvent) => {
      if (dragging.current) apply(vertical ? e.clientY : e.clientX);
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
  }, [vertical, location]);

  const split = paneIds.length === 2;
  const paneBox = (id: string, basis: number | null) => {
    const desc = descriptorFor(id);
    return (
      <div
        key={id}
        style={basis === null ? undefined : { flexBasis: `${basis * 100}%` }}
        className={`flex min-h-0 min-w-0 ${basis === null ? "flex-1" : ""} ${vertical ? "flex-col" : "flex-row"}`}
      >
        <PaneFrame
          title={desc.title}
          icon={desc.icon}
          dock={location}
          onMove={(dock) => onMove(id, dock)}
          onClose={() => onClose(id)}
        >
          {renderBody(id)}
        </PaneFrame>
      </div>
    );
  };

  return (
    <div ref={containerRef} className={`flex min-h-0 min-w-0 flex-1 ${vertical ? "flex-col" : "flex-row"}`}>
      {split ? (
        <>
          {paneBox(paneIds[0]!, ratio)}
          <div
            data-testid={`dock-resize-${location}`}
            onMouseDown={handleMouseDown}
            className={`shrink-0 hover:bg-brand-600/50 transition-colors duration-75 ${
              vertical ? "h-1 cursor-row-resize" : "w-1 cursor-col-resize"
            }`}
          />
          {paneBox(paneIds[1]!, 1 - ratio)}
        </>
      ) : (
        paneIds.map((id) => paneBox(id, null))
      )}
    </div>
  );
}
