import { useCallback, useEffect, useRef, useState } from "react";

import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const SPLIT_STORAGE_KEY = "aoe-split-ratio";
const DEFAULT_DIFF_WIDTH = 380;
const MIN_TERMINAL_WIDTH = 400;
const MIN_DIFF_WIDTH = 280;

interface Props {
  left: React.ReactNode;
  right: React.ReactNode;
  collapsed: boolean;
  onToggleCollapse: () => void;
}

function loadSavedWidth(): number {
  const saved = safeGetItem(SPLIT_STORAGE_KEY);
  if (saved) {
    const w = parseInt(saved, 10);
    if (w >= MIN_DIFF_WIDTH) return w;
  }
  return DEFAULT_DIFF_WIDTH;
}

export function ContentSplit({ left, right, collapsed, onToggleCollapse }: Props) {
  const [diffWidth, setDiffWidth] = useState(loadSavedWidth);
  const containerRef = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  }, []);

  useEffect(() => {
    const handleMouseMove = (e: MouseEvent) => {
      if (!dragging.current || !containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const newDiffWidth = rect.right - e.clientX;
      const terminalWidth = rect.width - newDiffWidth;

      if (newDiffWidth >= MIN_DIFF_WIDTH && terminalWidth >= MIN_TERMINAL_WIDTH) {
        setDiffWidth(newDiffWidth);
      }
    };

    const handleMouseUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      // Persist
      setDiffWidth((w) => {
        safeSetItem(SPLIT_STORAGE_KEY, String(w));
        return w;
      });
      // Trigger resize for terminal fit
      window.dispatchEvent(new Event("resize"));
    };

    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
    };
  }, []);

  // Re-fit terminal when collapsed state changes
  useEffect(() => {
    window.dispatchEvent(new Event("resize"));
  }, [collapsed]);

  return (
    <div ref={containerRef} className="flex-1 flex min-h-0 overflow-hidden relative">
      {/* Terminal pane */}
      <div className="flex-1 flex flex-col min-w-0 min-h-0">{left}</div>

      {!collapsed && (
        <>
          {/* Drag handle (desktop) */}
          <div
            data-testid="content-split-resize-handle"
            onMouseDown={handleMouseDown}
            onDoubleClick={onToggleCollapse}
            className="hidden md:block w-1 cursor-col-resize shrink-0 bg-surface-800 hover:bg-brand-600/50 transition-colors duration-75"
          />

          {/* Right pane (inline). ContentSplit only renders at the md
              breakpoint and up; below md the mobile picker promotes the
              chosen view into the single full-viewport pane instead (#1452). */}
          <div style={{ width: diffWidth }} className="flex shrink-0 flex-col min-h-0 overflow-hidden">
            {right}
          </div>
        </>
      )}
    </div>
  );
}
