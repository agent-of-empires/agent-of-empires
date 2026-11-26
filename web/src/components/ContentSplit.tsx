import { useCallback, useEffect, useRef, useState } from "react";

import { useFocusTrap } from "../hooks/useFocusTrap";
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
  /** Overlay the main area instead of taking a column; the backdrop closes it. */
  sheet?: boolean;
  /** The right pane takes the whole width, hiding (not unmounting) the left. */
  maximized?: boolean;
}

function loadSavedWidth(): number {
  const saved = safeGetItem(SPLIT_STORAGE_KEY);
  if (saved) {
    const w = parseInt(saved, 10);
    if (w >= MIN_DIFF_WIDTH) return w;
  }
  return DEFAULT_DIFF_WIDTH;
}

export function ContentSplit({ left, right, collapsed, onToggleCollapse, sheet = false, maximized = false }: Props) {
  const [diffWidth, setDiffWidth] = useState(loadSavedWidth);
  // Publish the live width so the TopBar's right zone can size itself to match the panel column and extend the
  // divider up through the header, mirroring the sidebar.
  useEffect(() => {
    document.documentElement.style.setProperty("--aoe-right-panel-width", `${diffWidth}px`);
  }, [diffWidth]);
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

  // Re-fit terminal when the panes' widths change
  useEffect(() => {
    window.dispatchEvent(new Event("resize"));
  }, [collapsed, sheet, maximized]);

  // An open sheet takes focus, and hands it back to where it was when it closes.
  const sheetOpen = sheet && !collapsed;
  const sheetRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!sheetOpen) return;
    const returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    sheetRef.current?.focus();
    return () => {
      if (returnTo?.isConnected) returnTo.focus();
    };
  }, [sheetOpen]);
  useFocusTrap(() => sheetRef.current, { active: sheetOpen });

  const inlineMaximized = maximized && !sheet;
  // Desktop panes are cards on the canvas, as in Claude Code desktop (see DESIGN.md).
  const dockBeside = !collapsed && !sheet && !inlineMaximized;
  return (
    <div ref={containerRef} className="flex-1 flex min-h-0 overflow-hidden relative">
      {/* Terminal pane */}
      <div
        className={`${inlineMaximized && !collapsed ? "hidden" : "flex"} flex-1 flex-col min-w-0 min-h-0 md:mb-2 md:ml-1 ${
          dockBeside ? "md:mr-1" : "md:mr-2"
        } md:rounded-xl md:border md:border-surface-700/45 md:bg-surface-900 md:shadow-md md:overflow-hidden`}
        inert={sheetOpen}
      >
        {left}
      </div>

      {sheetOpen && (
        <div
          data-testid="right-dock-sheet-backdrop"
          onClick={onToggleCollapse}
          className="absolute inset-0 z-30 bg-black/40 animate-fade-in"
        />
      )}

      {!collapsed && (
        <>
          {/* Drag handle (desktop) */}
          {!sheet && !inlineMaximized && (
            <div
              data-testid="content-split-resize-handle"
              onMouseDown={handleMouseDown}
              onDoubleClick={onToggleCollapse}
              className="hidden md:block w-1 cursor-col-resize shrink-0 hover:bg-brand-600/50 transition-colors duration-75"
            />
          )}

          {/* Right pane: one element in every mode, so its panes never remount on a mode change. */}
          <div
            ref={sheetRef}
            {...(sheet
              ? {
                  role: "dialog",
                  "aria-modal": true,
                  "aria-label": "Panels",
                  tabIndex: -1,
                  "data-testid": "right-dock-sheet",
                }
              : {})}
            style={
              sheet
                ? { width: `min(${Math.max(diffWidth, 420)}px, 88%)` }
                : inlineMaximized
                  ? undefined
                  : { width: diffWidth }
            }
            className={`flex flex-col min-h-0 overflow-hidden ${
              sheet
                ? "absolute inset-y-0 right-0 z-40 bg-surface-950 p-2 border-l border-surface-700/60 shadow-xl focus:outline-none"
                : inlineMaximized
                  ? "flex-1 min-w-0 md:mb-2 md:mx-2"
                  : "shrink-0 md:mb-2 md:mr-2 md:ml-1"
            }`}
          >
            {right}
          </div>
        </>
      )}
    </div>
  );
}
