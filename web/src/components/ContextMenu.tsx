import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  type RefObject,
} from "react";
import { Check } from "lucide-react";
import { CloseButton } from "./Dialog";
import { createPortal } from "react-dom";
import { useFocusTrap } from "../hooks/useFocusTrap";
import {
  BACKDROP,
  MENU,
  MENU_HEADING,
  MENU_HEADING_TEXT,
  MENU_ITEM,
  MENU_ITEM_DANGER,
  MENU_PHONE,
  MENU_SEPARATOR,
  SHEET,
  SHEET_HANDLE,
} from "../lib/surfaces";
import { moveMenuFocus } from "./menuFocus";

// Below Tailwind's `md`, where `sheetOnMobile` docks the menu as a sheet.
const MOBILE_QUERY = "(max-width: 767.98px)";
// How far the sheet's header must be dragged down to dismiss it.
const SWIPE_CLOSE_PX = 72;
/** A phone's floating menu is frosted; a menu docked as a sheet takes SHEET instead. */
/** MENU_HEADING's type, for a heading row that sets its own padding. */

export function ContextMenu({
  menu,
  menuRef,
  testId,
  minWidth = "min-w-[190px]",
  sheetOnMobile = false,
  role,
  label,
  heading,
  onClose,
  returnFocusTo,
  children,
}: {
  menu: { x: number; y: number };
  menuRef: RefObject<HTMLDivElement | null>;
  testId?: string;
  minWidth?: string;
  /** Below `md`, dock to the bottom edge over a backdrop instead of floating at the pointer. */
  sheetOnMobile?: boolean;
  /** For a menu whose items carry `menuitem` roles; a phone sheet is a dialog regardless. */
  role?: "menu";
  /** Accessible name of the sheet dialog. */
  label?: string;
  /** Sticky title row, set as a menu heading when it is plain text; on the phone sheet it carries the grab
   *  handle and a close button. */
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

  // Modal sheet: Tab and Shift+Tab wrap instead of leaving for the page behind.
  useFocusTrap(() => menuRef.current, { active: modal });
  const swipe = useSwipeToClose(menuRef, modal ? onClose : undefined);

  // `!` overrides the pointer position and height cap set inline.
  const sheet = sheetOnMobile
    ? " max-md:!left-0 max-md:!top-auto max-md:bottom-0 max-md:w-full max-md:!max-h-[85dvh] max-md:pb-[max(0.5rem,env(safe-area-inset-bottom))]"
    : "";
  const surface = modal ? `${SHEET} py-1` : `${MENU} ${MENU_PHONE}`;
  return createPortal(
    <>
      {/* Taps on the backdrop fall through to the hook's outside-click close. */}
      {sheetOnMobile && <div className={`md:hidden fixed inset-0 z-50 ${BACKDROP}`} aria-hidden="true" />}
      <div
        ref={menuRef}
        data-testid={testId}
        role={modal ? "dialog" : role}
        aria-modal={modal || undefined}
        aria-label={modal ? label : undefined}
        // The browser's own menu never opens over this one.
        onContextMenu={(e) => e.preventDefault()}
        onKeyDown={(e) => {
          if (e.key === "Escape" && onClose) {
            e.preventDefault();
            onClose();
          }
        }}
        className={`fixed z-50 ${surface} ${minWidth} overflow-y-auto overscroll-contain${sheet}`}
        style={{ left: menu.x, top: menu.y, maxHeight: "calc(100dvh - 16px)" }}
      >
        {(heading != null || sheetOnMobile) && (
          <div {...swipe} className="sticky top-0 z-10 bg-surface-800 max-md:-mt-1 max-md:touch-none">
            {sheetOnMobile && <div className={`md:hidden mb-1 ${SHEET_HANDLE}`} aria-hidden="true" />}
            <div className="flex items-center gap-2 px-3 pt-1.5 pb-1 max-md:pt-0">
              <div className="min-w-0 flex-1">
                {typeof heading === "string" ? <span className={MENU_HEADING_TEXT}>{heading}</span> : heading}
              </div>
              {sheetOnMobile && onClose && (
                <span className="md:hidden flex">
                  <CloseButton onClick={onClose} testId={`${testId}-close`} />
                </span>
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

/** One row of a menu. `role` and `checked` make it a menu item, or one of a set of choices with a check on
 *  the current one; icons take the menu's size whatever their own. */
export function MenuItem({
  onClick,
  testId,
  icon,
  indent = false,
  danger = false,
  role,
  checked,
  disabled,
  disabledBy,
  tabIndex,
  ariaExpanded,
  ariaControls,
  detail,
  trailing,
  className = "",
  children,
}: {
  onClick: (e: MouseEvent<HTMLButtonElement>) => void;
  testId?: string;
  icon?: ReactNode;
  indent?: boolean;
  /** For an item that removes or stops something. */
  danger?: boolean;
  role?: "menuitem" | "menuitemradio" | "menuitemcheckbox";
  checked?: boolean;
  disabled?: boolean;
  /** The id of a line saying why the item is unavailable. Unlike `disabled`, the item stays focusable and
   *  announced, as aria-disabled and described by that line, and a click does nothing. */
  disabledBy?: string;
  tabIndex?: number;
  /** For an item that discloses a group, e.g. "More". */
  ariaExpanded?: boolean;
  ariaControls?: string;
  /** A second line under the label; screen readers get it as the item's description. */
  detail?: ReactNode;
  /** Quiet text at the row's end, such as a state. */
  trailing?: ReactNode;
  /** Layout only, such as a sticky footer row; the look comes from MENU_ITEM. */
  className?: string;
  children: ReactNode;
}) {
  const detailId = useId();
  const base = danger ? MENU_ITEM_DANGER : MENU_ITEM;
  const look = indent ? base.replace("px-3", "pl-6 pr-3") : base;
  const describedBy = [disabledBy, detail ? detailId : undefined].filter(Boolean).join(" ");
  return (
    <button
      type="button"
      onClick={disabledBy ? undefined : onClick}
      data-testid={testId}
      role={role}
      aria-checked={checked}
      disabled={disabled}
      aria-disabled={disabledBy ? true : undefined}
      tabIndex={tabIndex}
      aria-expanded={ariaExpanded}
      aria-controls={ariaControls}
      aria-describedby={describedBy || undefined}
      className={`${disabledBy ? `${look.replace("cursor-pointer", "cursor-not-allowed")} opacity-40` : look} ${className}`.trimEnd()}
    >
      {icon != null && (
        <span className="flex shrink-0 [&>*]:size-4 max-md:[&>*]:size-[18px]" aria-hidden="true">
          {icon}
        </span>
      )}
      <span className="min-w-0 flex-1">
        <span className="block truncate">{children}</span>
        {/* Out of the name, so it reads once, as the description. */}
        {detail && (
          <span id={detailId} aria-hidden="true" className="block truncate text-[12px] text-text-dim">
            {detail}
          </span>
        )}
      </span>
      {trailing != null && <span className="shrink-0 font-mono text-[11px] text-text-dim">{trailing}</span>}
      {checked && <Check className="size-4 shrink-0 text-text-primary" strokeWidth={2} aria-hidden />}
    </button>
  );
}

export function MenuSeparator() {
  return <div role="separator" className={MENU_SEPARATOR} />;
}

/** `decorative` for a heading a labelled group already names. */
export function MenuHeading({ decorative = false, children }: { decorative?: boolean; children: ReactNode }) {
  return (
    <div className={MENU_HEADING} aria-hidden={decorative || undefined}>
      {children}
    </div>
  );
}

/** A labelled row of inline choices, replacing a heading plus one item per option. */
export function MenuChoiceRow({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="px-3 py-1.5">
      <div className="flex items-center gap-2">
        <span className="w-12 shrink-0 text-[12px] text-text-dim">{label}</span>
        <div className="flex flex-1 flex-wrap items-center">{children}</div>
      </div>
      {hint && <p className="mt-1 pl-14 text-[11px] text-text-dim">{hint}</p>}
    </div>
  );
}

/** A dropdown's list, placed by `className` against its trigger. Arrow, Home and End move among the items,
 *  and `initialFocus` picks the one focused as it opens. */
export function DropdownMenu({
  menuRef,
  id,
  label,
  labelledBy,
  describedBy,
  testId,
  className,
  style,
  initialFocus,
  phoneGlass = true,
  onKeyDown = moveMenuFocus,
  children,
}: {
  menuRef?: RefObject<HTMLDivElement | null>;
  id?: string;
  label?: string;
  labelledBy?: string;
  /** A line inside the menu that says why its choices are unavailable. */
  describedBy?: string;
  testId?: string;
  /** Position, width and height cap. */
  className: string;
  style?: CSSProperties;
  /** `checked` falls back to the first item when none is checked. */
  initialFocus?: "first" | "last" | "checked";
  /** Frosted on a phone, as DESIGN.md has a phone's menus; off for one inside a sheet or panel. */
  phoneGlass?: boolean;
  onKeyDown?: (e: KeyboardEvent<HTMLDivElement>) => void;
  children: ReactNode;
}) {
  const [focusAt] = useState(initialFocus);
  const attach = useCallback(
    (el: HTMLDivElement | null) => {
      if (menuRef) menuRef.current = el;
      if (!el || !focusAt || el.contains(document.activeElement)) return;
      const items = [...el.querySelectorAll<HTMLElement>(MENU_ITEMS)];
      const pick =
        focusAt === "last"
          ? items[items.length - 1]
          : ((focusAt === "checked" ? items.find((i) => i.getAttribute("aria-checked") === "true") : undefined) ??
            items[0]);
      pick?.focus({ preventScroll: true });
    },
    [menuRef, focusAt],
  );
  return (
    <div
      ref={attach}
      id={id}
      role="menu"
      aria-label={label}
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      data-testid={testId}
      data-owns-escape="open"
      onKeyDown={onKeyDown}
      style={style}
      className={`${MENU} ${phoneGlass ? MENU_PHONE : ""} overflow-y-auto overscroll-contain animate-fade-in ${className}`}
    >
      {children}
    </div>
  );
}

const MENU_ITEMS = '[role^="menuitem"]:not(:disabled)';
