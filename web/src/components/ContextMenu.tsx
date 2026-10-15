import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
  type RefObject,
} from "react";
import { X } from "lucide-react";
import { createPortal } from "react-dom";

// Below Tailwind's `md`, where `sheetOnMobile` docks the menu as a sheet.
const MOBILE_QUERY = "(max-width: 767.98px)";
// How far the sheet's header must be dragged down to dismiss it.
const SWIPE_CLOSE_PX = 72;

export function ContextMenu({
  menu,
  menuRef,
  testId,
  minWidth = "min-w-[190px]",
  sheetOnMobile = false,
  label,
  heading,
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
  /** Sticky title row; on the phone sheet it carries the grab handle and a close button. */
  heading?: ReactNode;
  /** Escape, the close button, and a swipe down on the sheet header close it. */
  onClose?: () => void;
  /** Where focus returns on close, when nothing else took it. */
  returnFocusTo?: RefObject<HTMLElement | null>;
  children: ReactNode;
}) {
  // Only the phone layout is a modal sheet; the desktop menu stays a plain floating menu.
  const [modal] = useState(
    () => sheetOnMobile && typeof window !== "undefined" && !!window.matchMedia?.(MOBILE_QUERY).matches,
  );
  // The mode is fixed at open, so crossing the breakpoint closes the menu rather than
  // leaving a desktop menu modal or a phone sheet without containment.
  useEffect(() => {
    if (!sheetOnMobile || !onClose || typeof window === "undefined") return;
    const query = window.matchMedia?.(MOBILE_QUERY);
    query?.addEventListener?.("change", onClose);
    return () => query?.removeEventListener?.("change", onClose);
  }, [sheetOnMobile, onClose]);
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

  const swipe = useSwipeToClose(menuRef, modal ? onClose : undefined);

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
        {(heading != null || sheetOnMobile) && (
          <div {...swipe} className="sticky top-0 z-10 bg-surface-800 max-md:-mt-1 max-md:touch-none">
            {sheetOnMobile && (
              <div className="md:hidden flex justify-center pt-2 pb-1" aria-hidden="true">
                <span className="h-1 w-9 rounded-full bg-surface-600" />
              </div>
            )}
            <div className="flex items-center gap-2 px-3 pt-1.5 pb-1 max-md:pt-0">
              <div className="min-w-0 flex-1">{heading}</div>
              {sheetOnMobile && onClose && (
                <button
                  type="button"
                  onClick={onClose}
                  data-testid={`${testId}-close`}
                  aria-label="Close"
                  className="md:hidden shrink-0 -mr-1.5 flex h-10 w-10 items-center justify-center rounded-md text-text-muted hover:bg-surface-700/50 hover:text-text-primary cursor-pointer transition-colors"
                >
                  <X className="h-5 w-5" />
                </button>
              )}
            </div>
          </div>
        )}
        {children}
      </div>
    </>,
    document.body,
  );
}

/** Drags the sheet with a finger on its header and closes it past `SWIPE_CLOSE_PX`. */
function useSwipeToClose(menuRef: RefObject<HTMLDivElement | null>, onClose: (() => void) | undefined) {
  const startY = useRef<number | null>(null);
  const offset = (dy: number, animate: boolean) => {
    const el = menuRef.current;
    if (!el) return;
    el.style.transition = animate ? "transform 150ms ease-out" : "";
    el.style.transform = dy > 0 ? `translateY(${dy}px)` : "";
  };
  if (!onClose) return {};
  return {
    onTouchStart: (e: React.TouchEvent) => {
      startY.current = e.touches[0]?.clientY ?? null;
    },
    onTouchMove: (e: React.TouchEvent) => {
      const y = e.touches[0]?.clientY;
      if (startY.current == null || y == null) return;
      offset(y - startY.current, false);
    },
    onTouchEnd: (e: React.TouchEvent) => {
      const y = e.changedTouches[0]?.clientY;
      const dy = startY.current == null || y == null ? 0 : y - startY.current;
      startY.current = null;
      if (dy > SWIPE_CLOSE_PX) onClose();
      else offset(0, true);
    },
    onTouchCancel: () => {
      startY.current = null;
      offset(0, true);
    },
  };
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

/** A labelled row of inline choices, replacing a heading plus one item per option. */
export function MenuChoiceRow({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="px-3 py-1.5">
      <div className="flex items-center gap-2">
        <span className="w-12 shrink-0 text-xs text-text-dim">{label}</span>
        <div className="flex flex-1 flex-wrap items-center">{children}</div>
      </div>
      {hint && <p className="mt-1 pl-14 text-[11px] text-text-dim">{hint}</p>}
    </div>
  );
}

export interface SwatchOption<K extends string> {
  key: K;
  label: string;
  className?: string;
  style?: CSSProperties;
}

/** Color picker: a dashed "none" swatch, then each color; the current pick is ringed. Testids are
 *  `${testIdPrefix}-${key}` and `${testIdPrefix}-clear`. */
export function MenuSwatches<K extends string>({
  options,
  value,
  onPick,
  testIdPrefix,
}: {
  options: SwatchOption<K>[];
  value: K | null;
  onPick: (key: K | null) => void;
  testIdPrefix: string;
}) {
  const swatch = (key: K | null, label: string, fill: ReactNode) => (
    <button
      key={key ?? "none"}
      type="button"
      onClick={() => onPick(key)}
      data-testid={`${testIdPrefix}-${key ?? "clear"}`}
      title={label}
      aria-label={label}
      aria-pressed={value === key}
      className="flex h-8 w-8 max-md:h-10 max-md:w-10 items-center justify-center rounded-md cursor-pointer hover:bg-surface-700/50 transition-colors"
    >
      <span
        className={`flex h-5 w-5 rounded-full ${
          value === key ? "ring-2 ring-text-primary ring-offset-2 ring-offset-surface-800" : ""
        }`}
      >
        {fill}
      </span>
    </button>
  );
  return (
    <>
      {swatch(null, "No color", <span className="h-full w-full rounded-full border border-dashed border-text-dim" />)}
      {options.map((o) =>
        swatch(o.key, o.label, <span className={`h-full w-full rounded-full ${o.className ?? ""}`} style={o.style} />),
      )}
    </>
  );
}
