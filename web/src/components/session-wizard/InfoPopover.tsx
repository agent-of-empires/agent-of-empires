import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Info } from "lucide-react";
import { useClampedMenuPosition } from "../../lib/menuPosition";

/**
 * An info icon that opens a small non-modal dialog. Unlike the shared Tooltip it takes clicks and
 * focus, so the panel can hold a link.
 */
export function InfoPopover({ title, children }: { title: string; children: ReactNode }) {
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const panelId = useId();
  const titleId = useId();
  useClampedMenuPosition(pos, panelRef, setPos);

  const open = pos !== null;
  const close = (returnFocus: boolean) => {
    setPos(null);
    if (returnFocus) buttonRef.current?.focus();
  };

  useEffect(() => {
    if (!open) return;
    // Non-modal, so it takes focus itself: Escape then reaches it and Tab goes on to the link.
    panelRef.current?.focus({ preventScroll: true });
    const onPointerDown = (e: PointerEvent) => {
      const target = e.target as Node;
      if (!panelRef.current?.contains(target) && !buttonRef.current?.contains(target)) setPos(null);
    };
    // It's placed once, so a scroll under it (the wizard's list) or a narrower window would leave it behind.
    const onScroll = (e: Event) => {
      if (!panelRef.current?.contains(e.target as Node)) setPos(null);
    };
    // Width only: a phone's keyboard and address bar resize the height, and opening this can close the keyboard.
    const openWidth = window.innerWidth;
    const onResize = () => {
      if (window.innerWidth !== openWidth) setPos(null);
    };
    document.addEventListener("pointerdown", onPointerDown, true);
    document.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onResize);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      document.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onResize);
    };
  }, [open]);

  const toggle = () => {
    if (open) return close(false);
    const rect = buttonRef.current?.getBoundingClientRect();
    setPos({ x: rect?.left ?? 0, y: (rect?.bottom ?? 0) + 4 });
  };

  return (
    <>
      <button
        ref={buttonRef}
        type="button"
        onClick={toggle}
        aria-label={title}
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        // 44px to the touch on phones; the negative margins keep the row's height and label column.
        className="inline-flex shrink-0 items-center justify-center h-11 w-11 -my-3.5 -mr-3.5 md:h-6 md:w-6 md:my-0 md:mr-0 rounded-md text-text-dim hover:text-text-secondary cursor-pointer"
      >
        <Info className="h-3.5 w-3.5" aria-hidden="true" />
      </button>
      {open &&
        createPortal(
          <div
            ref={panelRef}
            id={panelId}
            role="dialog"
            aria-labelledby={titleId}
            // The app's capture-phase Escape would otherwise close the wizard too.
            data-owns-escape="open"
            tabIndex={-1}
            onKeyDown={(e) => {
              if (e.key !== "Escape") return;
              e.preventDefault();
              close(true);
            }}
            // Above the wizard's z-[60] overlay.
            className="fixed z-[70] w-[min(20rem,calc(100vw-16px))] rounded-lg border border-surface-700 bg-surface-800 p-3 text-sm shadow-lg outline-none"
            style={{ left: pos.x, top: pos.y }}
          >
            <p id={titleId} className="font-medium text-text-primary">
              {title}
            </p>
            {children}
          </div>,
          document.body,
        )}
    </>
  );
}
