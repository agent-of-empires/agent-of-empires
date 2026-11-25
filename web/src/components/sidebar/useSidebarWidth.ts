import { useEffect, useEffectEvent, useRef, useState } from "react";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";

const WIDTH_KEY = "aoe-sidebar-width";
const DEFAULT_WIDTH = 280;
const MIN_WIDTH = 200;
const MAX_WIDTH = 480;
/** The compact rail overrides, but does not overwrite, the dragged width. */
const COMPACT_WIDTH = 88;
/** A drag snaps to whichever state is nearer: the rail below the midpoint of rail and minimum, closed below half the rail. */
const COMPACT_SNAP = (COMPACT_WIDTH + MIN_WIDTH) / 2;
const CLOSE_SNAP = COMPACT_WIDTH / 2;
const KEY_STEP = 16;

export type SidebarSnap = { mode: "full"; width: number } | { mode: "compact" } | { mode: "closed" };

const clampWidth = (w: number) => Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, Math.round(w)));

/** Where a drag ends up with the pointer `x` pixels from the sidebar's outer edge. */
export function snapForPointer(x: number): SidebarSnap {
  if (x < CLOSE_SNAP) return { mode: "closed" };
  if (x < COMPACT_SNAP) return { mode: "compact" };
  return { mode: "full", width: clampWidth(x) };
}

export type SidebarLayout = { compact: boolean; width: number };

/**
 * The layout a key on the resize handle moves to, or null for a key it ignores. Narrowing stops at the rail;
 * closing stays with the top bar toggle so the focused handle never disappears.
 */
export function layoutForKey(layout: SidebarLayout, key: string): SidebarLayout | null {
  const { compact, width } = layout;
  switch (key) {
    case "ArrowLeft":
      if (compact) return null;
      return width <= MIN_WIDTH ? { compact: true, width } : { compact, width: clampWidth(width - KEY_STEP) };
    case "ArrowRight":
      return compact ? { compact: false, width } : { compact, width: clampWidth(width + KEY_STEP) };
    case "Home":
      return { compact: true, width };
    case "End":
      return { compact: false, width: MAX_WIDTH };
    // The ARIA window splitter pattern: Enter collapses the pane, or restores it to where it was.
    case "Enter":
      return { compact: !compact, width };
    default:
      return null;
  }
}

function loadSavedWidth(): number {
  const saved = safeGetItem(WIDTH_KEY);
  if (saved) {
    const w = parseInt(saved, 10);
    if (w >= MIN_WIDTH && w <= MAX_WIDTH) return w;
  }
  return DEFAULT_WIDTH;
}

type DragStart = { pointerId: number; left: number; width: number; compact: boolean; last: SidebarSnap | null };

/**
 * Drag-resizable sidebar, published as `--aoe-sidebar-width` for the TopBar. A drag follows the pointer down to
 * the minimum width, then snaps to the compact rail and to closed, and back as the pointer returns. Only a drag
 * that ends at a width saves it; one that ends closed restores the width and mode it started from, so reopening
 * from the top bar brings back the sidebar the user had.
 */
export function useSidebarWidth({
  compact,
  onCompactChange,
  open,
  onOpenChange,
}: {
  compact: boolean;
  onCompactChange: (compact: boolean) => void;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [width, setWidth] = useState(loadSavedWidth);
  const effectiveWidth = compact ? COMPACT_WIDTH : width;
  const panelRef = useRef<HTMLDivElement>(null);
  const drag = useRef<DragStart | null>(null);

  useEffect(() => {
    document.documentElement.style.setProperty("--aoe-sidebar-width", `${effectiveWidth}px`);
  }, [effectiveWidth]);

  const apply = (next: SidebarLayout & { open: boolean }) => {
    setWidth(next.width);
    if (next.compact !== compact) onCompactChange(next.compact);
    if (next.open !== open) onOpenChange(next.open);
  };

  const onPointerDown = (e: React.PointerEvent) => {
    // A second contact cannot take over a drag; a mouse can restart its own after a lost release.
    if (e.button !== 0 || (drag.current && drag.current.pointerId !== e.pointerId)) return;
    e.preventDefault();
    drag.current = {
      pointerId: e.pointerId,
      left: panelRef.current?.getBoundingClientRect().left ?? 0,
      width,
      compact,
      last: null,
    };
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  };

  const onDragEnd = useEffectEvent((e: PointerEvent) => {
    const d = drag.current;
    if (!d || e.pointerId !== d.pointerId) return;
    drag.current = null;
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    const saved = d.last?.mode === "full" ? d.last.width : d.width;
    setWidth(saved);
    safeSetItem(WIDTH_KEY, String(saved));
  });

  const onDragMove = useEffectEvent((e: PointerEvent) => {
    const d = drag.current;
    if (!d || e.pointerId !== d.pointerId) return;
    // A mouse released outside the window sends no pointerup.
    if (e.pointerType === "mouse" && e.buttons === 0) return onDragEnd(e);
    const snap = snapForPointer(e.clientX - d.left);
    d.last = snap;
    if (snap.mode === "closed") apply({ compact: d.compact, width: d.width, open: false });
    else apply({ compact: snap.mode === "compact", width: snap.mode === "full" ? snap.width : d.width, open: true });
  });

  useEffect(() => {
    const onMove = (e: PointerEvent) => onDragMove(e);
    const onUp = (e: PointerEvent) => onDragEnd(e);
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
    document.addEventListener("pointercancel", onUp);
    return () => {
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
      document.removeEventListener("pointercancel", onUp);
    };
  }, []);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const next = layoutForKey({ compact, width }, e.key);
    if (!next) return;
    e.preventDefault();
    apply({ ...next, open: true });
    safeSetItem(WIDTH_KEY, String(next.width));
  };

  const onDoubleClick = () => {
    apply({ compact: false, width: DEFAULT_WIDTH, open: true });
    safeSetItem(WIDTH_KEY, String(DEFAULT_WIDTH));
  };

  const handleProps = {
    role: "separator",
    tabIndex: 0,
    "aria-orientation": "vertical",
    "aria-label": "Resize sidebar",
    "aria-valuemin": COMPACT_WIDTH,
    "aria-valuemax": MAX_WIDTH,
    "aria-valuenow": effectiveWidth,
    onPointerDown,
    onKeyDown,
    onDoubleClick,
  } as const;

  return { effectiveWidth, panelRef, handleProps };
}
