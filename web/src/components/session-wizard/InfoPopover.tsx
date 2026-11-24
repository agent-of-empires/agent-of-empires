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
  // Inside the modal around the icon, such as the wizard, so its focus trap counts the link as its own.
  const [host, setHost] = useState<HTMLElement>(() => document.body);
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
    const outside = (target: EventTarget | null) =>
      !panelRef.current?.contains(target as Node) && !buttonRef.current?.contains(target as Node);
    const onPointerDown = (e: PointerEvent) => {
      if (outside(e.target)) setPos(null);
    };
    // Shift+Tab out of the portalled panel would leave it open over the wizard, where Escape closes the wizard.
    const onFocusIn = (e: FocusEvent) => {
      if (outside(e.target)) setPos(null);
    };
    // Closing under focus would drop it to the body and lose Tab's place in the wizard, so focus goes back
    // to the icon. Focus elsewhere stays where it is.
    const dismiss = () => {
      const focused = panelRef.current?.contains(document.activeElement) ?? false;
      setPos(null);
      if (focused) buttonRef.current?.focus({ preventScroll: true });
    };
    // It's placed once, so a scroll under it (the wizard's list) would leave it behind.
    const onScroll = (e: Event) => {
      if (!panelRef.current?.contains(e.target as Node)) dismiss();
    };
    // A narrower window moves the icon, so close. A height change (a phone's keyboard or address bar, which
    // opening this can trigger) keeps it open, re-clamped: the clamp hook only watches the panel's own size.
    const openWidth = window.innerWidth;
    const onResize = () => {
      if (window.innerWidth !== openWidth) dismiss();
      else setPos((p) => p && { ...p });
    };
    document.addEventListener("pointerdown", onPointerDown, true);
    document.addEventListener("focusin", onFocusIn, true);
    document.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onResize);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      document.removeEventListener("focusin", onFocusIn, true);
      document.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onResize);
    };
  }, [open]);

  const toggle = () => {
    if (open) return close(false);
    const rect = buttonRef.current?.getBoundingClientRect();
    setHost(buttonRef.current?.closest<HTMLElement>('[aria-modal="true"]') ?? document.body);
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
          host,
        )}
    </>
  );
}
