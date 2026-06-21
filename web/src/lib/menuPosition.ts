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

/** Clamp a `position: fixed` floating menu's top-left so the menu fits
 *  inside the viewport with at least `margin` pixels of breathing room
 *  on every side. Used by the sidebar's right-click / long-press
 *  context menus so opening one near the bottom or right edge does not
 *  push items off-screen. When the menu is taller or wider than the
 *  viewport (minus margins) the position collapses to `margin` rather
 *  than going negative; the menu's own stylesheet caps `max-height`
 *  with `overflow-y: auto` to make the tail scrollable. See #1601. */
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

/** React hook: keep a floating context menu inside the viewport.
 *
 *  On every state transition of `contextMenu`, measure `menuRef` after
 *  layout, call `clampMenuPosition`, and forward the clamped position
 *  through `setContextMenu` if it differs. Wires a `ResizeObserver`
 *  while the menu is open so a deferred layout shift (web fonts
 *  loading, icon images decoding) re-runs the clamp instead of leaking
 *  items off the bottom edge. Guarded with a `typeof` check so the
 *  jsdom-based Vitest suites that lack `ResizeObserver` still execute
 *  the layout effect cleanly. See #1601. */
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
        // Merge so callers carrying extra menu state (e.g. a single/bulk
        // scope) keep it through a reposition. See #2312.
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
