import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useClampedMenuPosition } from "../../lib/menuPosition";
import { writeClipboard } from "../../lib/clipboard";
import { toastBus } from "../../lib/toastBus";

export interface PathMenuState {
  x: number;
  y: number;
  /** Repo-relative path of the right-clicked file or folder. */
  path: string;
}

interface Props {
  menu: PathMenuState | null;
  onClose: () => void;
}

/** A minimal right-click menu for the diff file list offering "Copy relative
 *  path". Rendered in a portal at the click position, clamped inside the
 *  viewport, and dismissed on any outside click, another right-click, or
 *  Escape. */
export function CopyPathContextMenu({ menu, onClose }: Props) {
  const menuRef = useRef<HTMLDivElement | null>(null);

  // Mirror the click position into local state so the clamp hook can nudge it
  // inside the viewport. The initial open is seeded by the lazy initializer;
  // render-time re-seeds on reopen at a new spot, both before paint so the
  // clamp hook below lands the menu on-screen without flashing at the raw
  // coordinates first.
  const [pos, setPos] = useState<{ x: number; y: number } | null>(
    menu ? { x: menu.x, y: menu.y } : null,
  );
  const [trackedMenu, setTrackedMenu] = useState(menu);
  if (menu !== trackedMenu) {
    setTrackedMenu(menu);
    setPos(menu ? { x: menu.x, y: menu.y } : null);
  }
  useClampedMenuPosition(pos, menuRef, setPos);

  useEffect(() => {
    if (!menu) return;
    const close = () => onClose();
    const onDocClick = (e: MouseEvent) => {
      // Clicks inside the menu are handled by the item's onClick.
      if (menuRef.current?.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    // Defer so the contextmenu event that opened this menu finishes bubbling
    // first; otherwise React flushes this effect synchronously for the discrete
    // event and the same right-click immediately hits the document listener,
    // closing the menu before it ever paints.
    const raf = requestAnimationFrame(() => {
      document.addEventListener("click", onDocClick);
      document.addEventListener("contextmenu", close);
      document.addEventListener("keydown", onKey);
    });
    return () => {
      cancelAnimationFrame(raf);
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("contextmenu", close);
      document.removeEventListener("keydown", onKey);
    };
  }, [menu, onClose]);

  if (!menu || !pos) return null;

  const path = menu.path;
  const copy = () => {
    void writeClipboard(path).then((ok) => {
      if (ok) toastBus.handler?.info(`Copied ${path}`);
      else toastBus.handler?.error("Couldn't copy path to clipboard");
    });
    onClose();
  };

  return createPortal(
    <div
      ref={menuRef}
      role="menu"
      className="fixed z-50 min-w-[160px] rounded-md border border-surface-700 bg-surface-850 py-1 shadow-lg"
      style={{ left: pos.x, top: pos.y }}
      onContextMenu={(e) => e.preventDefault()}
    >
      <button
        type="button"
        role="menuitem"
        onClick={copy}
        className="w-full px-3 py-1.5 text-left text-[13px] text-text-secondary hover:bg-surface-800 cursor-pointer"
      >
        Copy relative path
      </button>
    </div>,
    document.body,
  );
}
