import { useCallback, useEffect, useRef, useState } from "react";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";

const WIDTH_KEY = "aoe-sidebar-width";
const DEFAULT_WIDTH = 280;
const MIN_WIDTH = 200;
const MAX_WIDTH = 480;
/** The compact rail overrides, but does not overwrite, the dragged width. */
const COMPACT_WIDTH = 88;

function loadSavedWidth(): number {
  const saved = safeGetItem(WIDTH_KEY);
  if (saved) {
    const w = parseInt(saved, 10);
    if (w >= MIN_WIDTH && w <= MAX_WIDTH) return w;
  }
  return DEFAULT_WIDTH;
}

/** Drag-resizable sidebar width, persisted on release and published as `--aoe-sidebar-width` for the TopBar. */
export function useSidebarWidth(compact: boolean) {
  const [width, setWidth] = useState(loadSavedWidth);
  const dragging = useRef(false);
  const effectiveWidth = compact ? COMPACT_WIDTH : width;

  useEffect(() => {
    document.documentElement.style.setProperty("--aoe-sidebar-width", `${effectiveWidth}px`);
  }, [effectiveWidth]);

  const startResize = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  }, []);

  useEffect(() => {
    const onMove = (e: MouseEvent) => {
      if (dragging.current) setWidth(Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, e.clientX)));
    };
    const onUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setWidth((w) => {
        safeSetItem(WIDTH_KEY, String(w));
        return w;
      });
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    return () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
    };
  }, []);

  return { effectiveWidth, startResize };
}
