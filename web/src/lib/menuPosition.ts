import { useLayoutEffect, type Dispatch, type RefObject, type SetStateAction } from "react";

export interface ClampMenuArgs {
  x: number;
  y: number;
  menuWidth: number;
  menuHeight: number;
  viewportWidth: number;
  viewportHeight: number;
  margin?: number;
}

/** Clamp a fixed menu inside the viewport with `margin` on every side, collapsing to `margin` when it cannot fit (the menu scrolls). */
export function clampMenuPosition({
  x,
  y,
  menuWidth,
  menuHeight,
  viewportWidth,
  viewportHeight,
  margin = 8,
}: ClampMenuArgs): { x: number; y: number } {
  const maxX = Math.max(margin, viewportWidth - menuWidth - margin);
  const maxY = Math.max(margin, viewportHeight - menuHeight - margin);
  const nextX = Math.min(Math.max(x, margin), maxX);
  const nextY = Math.min(Math.max(y, margin), maxY);
  return { x: nextX, y: nextY };
}

/** Re-clamp after layout and on resize (late fonts or icons), skipping `ResizeObserver` where unavailable. */
export function useClampedMenuPosition<T extends { x: number; y: number }>(
  contextMenu: T | null,
  menuRef: RefObject<HTMLElement | null>,
  setContextMenu: Dispatch<SetStateAction<T | null>>,
): void {
  useLayoutEffect(() => {
    if (!contextMenu || !menuRef.current) return;
    const menu = menuRef.current;
    const clamp = () => {
      const rect = menu.getBoundingClientRect();
      const next = clampMenuPosition({
        x: contextMenu.x,
        y: contextMenu.y,
        menuWidth: rect.width,
        menuHeight: rect.height,
        viewportWidth: window.innerWidth,
        viewportHeight: window.innerHeight,
      });
      if (next.x !== contextMenu.x || next.y !== contextMenu.y) {
        // Merge so callers keep extra menu state through a reposition.
        setContextMenu((prev) => (prev ? { ...prev, x: next.x, y: next.y } : prev));
      }
    };
    clamp();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(clamp);
    ro.observe(menu);
    return () => ro.disconnect();
  }, [contextMenu, menuRef, setContextMenu]);
}
