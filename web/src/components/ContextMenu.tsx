import { useLayoutEffect, useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";

// Below Tailwind's `md`, where `sheetOnMobile` docks the menu as a sheet.
const MOBILE_QUERY = "(max-width: 767.98px)";

export function ContextMenu({
  menu,
  menuRef,
  testId,
  minWidth = "min-w-[190px]",
  sheetOnMobile = false,
  label,
  onClose,
  returnFocusTo,
  children,
}: {
  menu: { x: number; y: number };
  menuRef: RefObject<HTMLDivElement | null>;
  testId: string;
  minWidth?: string;
  /** Below `md`, dock to the bottom edge over a backdrop instead of floating at the pointer. */
  sheetOnMobile?: boolean;
  /** Accessible name of the sheet dialog. */
  label?: string;
  /** Escape closes the sheet. */
  onClose?: () => void;
  /** Where focus returns on close, when nothing else took it. */
  returnFocusTo?: RefObject<HTMLElement | null>;
  children: ReactNode;
}) {
  // Only the phone layout is a modal sheet; the desktop menu stays a plain floating menu.
  const [modal] = useState(
    () => sheetOnMobile && typeof window !== "undefined" && !!window.matchMedia?.(MOBILE_QUERY).matches,
  );
  // Focus enters on open and goes back to the trigger on close, unless the closing click
  // already focused something else.
  useLayoutEffect(() => {
    if (!modal) return;
    const el = menuRef.current;
    const trigger = returnFocusTo?.current;
    el?.querySelector<HTMLElement>("button:not([disabled])")?.focus({ preventScroll: true });
    return () => {
      const active = document.activeElement;
      if (!active || active === document.body || el?.contains(active)) {
        trigger?.focus({ preventScroll: true });
      }
    };
  }, [modal, menuRef, returnFocusTo]);

  // `!` overrides the pointer position and height cap set inline.
  const sheet = sheetOnMobile
    ? " max-md:!left-0 max-md:!top-auto max-md:bottom-0 max-md:w-full max-md:!max-h-[85dvh] max-md:rounded-b-none max-md:border-x-0 max-md:border-b-0 max-md:pb-[max(0.5rem,env(safe-area-inset-bottom))]"
    : "";
  return createPortal(
    <>
      {/* Taps on the backdrop fall through to the hook's outside-click close. */}
      {sheetOnMobile && <div className="md:hidden fixed inset-0 z-50 bg-black/50" aria-hidden="true" />}
      <div
        ref={menuRef}
        data-testid={testId}
        role={modal ? "dialog" : undefined}
        aria-modal={modal || undefined}
        aria-label={modal ? label : undefined}
        onKeyDown={(e) => {
          if (e.key === "Escape" && onClose) {
            e.preventDefault();
            onClose();
          }
          // Modal sheet: Tab and Shift+Tab wrap instead of leaving for the page behind.
          if (e.key === "Tab" && modal) {
            const items = [...(menuRef.current?.querySelectorAll<HTMLElement>("button:not([disabled])") ?? [])].filter(
              (el) => !el.closest("[hidden]"),
            );
            const first = items[0];
            const last = items[items.length - 1];
            const wrapTo = e.shiftKey
              ? document.activeElement === first && last
              : document.activeElement === last && first;
            if (wrapTo) {
              e.preventDefault();
              wrapTo.focus();
            }
          }
        }}
        className={`fixed z-50 bg-surface-800 border border-surface-700 rounded-lg shadow-lg py-1 ${minWidth} overflow-y-auto${sheet}`}
        style={{ left: menu.x, top: menu.y, maxHeight: "calc(100dvh - 16px)" }}
      >
        {children}
      </div>
    </>,
    document.body,
  );
}

export function MenuItem({
  onClick,
  testId,
  icon,
  indent = false,
  flex = icon != null,
  className = "text-text-secondary hover:bg-surface-700/50",
  ariaExpanded,
  ariaControls,
  children,
}: {
  onClick: () => void;
  testId?: string;
  /** For an item that discloses a group, e.g. "More". */
  ariaExpanded?: boolean;
  ariaControls?: string;
  icon?: ReactNode;
  indent?: boolean;
  flex?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      data-testid={testId}
      aria-expanded={ariaExpanded}
      aria-controls={ariaControls}
      className={`w-full text-left ${indent ? "pl-6 pr-3" : "px-3"} py-2 md:py-2 max-md:py-3 text-sm ${className} cursor-pointer transition-colors${flex ? " flex items-center gap-2" : ""}`}
    >
      {icon}
      {children}
    </button>
  );
}

export function MenuSeparator() {
  return <div className="border-t border-surface-700/20 my-1" />;
}

export function MenuHeading({ children }: { children: ReactNode }) {
  return <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">{children}</div>;
}
