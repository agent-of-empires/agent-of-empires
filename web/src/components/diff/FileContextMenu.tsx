import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useClampedMenuPosition } from "../../lib/menuPosition";
import { writeClipboard } from "../../lib/clipboard";
import { openInNewTab } from "../../lib/openInNewTab";
import { toastBus } from "../../lib/toastBus";

export interface PathMenuState {
  x: number;
  y: number;
  path: string;
  /** Where Open file reads the file's current bytes; a directory row has none and only copies its path. */
  open?: { url: string; disabled?: boolean };
}

interface Props {
  menu: PathMenuState | null;
  onClose: () => void;
}

const ITEM =
  "w-full px-3 py-1.5 text-left text-[13px] text-text-secondary hover:bg-surface-800 cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent";

function openFailureMessage(path: string, status?: number): string {
  if (status === 404) return `${path} is not in the worktree`;
  if (status === 413) return `${path} is too large to open (over 50 MiB)`;
  return `Couldn't open ${path}`;
}

/** File-row actions at the click position, clamped to the viewport. */
export function FileContextMenu({ menu, onClose }: Props) {
  const menuRef = useRef<HTMLDivElement | null>(null);

  // Local position so the clamp can nudge it on-screen before paint.
  const [pos, setPos] = useState<{ x: number; y: number } | null>(menu ? { x: menu.x, y: menu.y } : null);
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
      if (menuRef.current?.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    // Deferred so the opening right-click does not immediately close the menu.
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

  const { path, open } = menu;
  const copy = () => {
    void writeClipboard(path).then((ok) => {
      if (ok) toastBus.handler?.info(`Copied ${path}`);
      else toastBus.handler?.error("Couldn't copy path to clipboard");
    });
    onClose();
  };
  const openFile = (url: string) => {
    void openInNewTab(url, path.slice(path.lastIndexOf("/") + 1)).then((result) => {
      if (!result.ok) toastBus.handler?.error(openFailureMessage(path, result.status));
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
      {open && (
        <button
          type="button"
          role="menuitem"
          disabled={open.disabled}
          onClick={() => openFile(open.url)}
          className={ITEM}
        >
          Open file
        </button>
      )}
      <button type="button" role="menuitem" onClick={copy} className={ITEM}>
        Copy relative path
      </button>
    </div>,
    document.body,
  );
}
