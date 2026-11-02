import {
  cloneElement,
  isValidElement,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { clampMenuPosition } from "../lib/menuPosition";

// Shared hover tooltip used by the sidebar control row (grouping axis, filter, new session) and the sort picker.
export function Tooltip({
  text,
  detail,
  children,
  multiline = false,
  id,
  open: controlledOpen,
}: {
  text: string;
  /** A second, dimmer line. */
  detail?: string;
  children: ReactNode;
  // Single-line callers (sidebar, sort picker) keep the default `whitespace-nowrap` pill.
  multiline?: boolean;
  /** The bubble's id, for a caller that describes its own focus target with it. */
  id?: string;
  /**
   * Set when the caller decides when it shows, e.g. a mark inside a focusable row: the row,
   * not the mark, takes focus, and carries `aria-describedby` itself.
   */
  open?: boolean;
}) {
  const triggerRef = useRef<HTMLSpanElement>(null);
  const tipRef = useRef<HTMLSpanElement>(null);
  const ownId = useId();
  const tipId = id ?? ownId;
  const [ownOpen, setOpen] = useState(false);
  const controlled = controlledOpen !== undefined;
  const open = controlled ? controlledOpen : ownOpen;
  const touchRef = useRef(false);
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null);

  const show = () => {
    setPos(null);
    setOpen(true);
  };
  const hide = () => {
    setOpen(false);
    setPos(null);
  };

  // Measure the trigger and the tooltip, center the tooltip below the trigger, and clamp it inside the viewport.
  const place = () => {
    if (!triggerRef.current || !tipRef.current) return;
    const anchor = triggerRef.current.getBoundingClientRect();
    const tip = tipRef.current.getBoundingClientRect();
    setPos(
      clampMenuPosition({
        x: anchor.left + anchor.width / 2 - tip.width / 2,
        y: anchor.bottom + 6,
        menuWidth: tip.width,
        menuHeight: tip.height,
        viewportWidth: window.innerWidth,
        viewportHeight: window.innerHeight,
      }),
    );
  };
  useLayoutEffect(() => {
    if (open) place();
  }, [open, text, detail]);

  // A fixed tooltip detaches from its trigger when an ancestor scrolls or the window resizes. An uncontrolled
  // one is dismissed. A controlled one follows its anchor, since focusing a row scrolls it into view and
  // hiding then would drop the row's description.
  useEffect(() => {
    if (!open) return;
    const onMove = controlled ? place : hide;
    window.addEventListener("scroll", onMove, true);
    window.addEventListener("resize", onMove);
    return () => {
      window.removeEventListener("scroll", onMove, true);
      window.removeEventListener("resize", onMove);
    };
  }, [open, controlled]);

  // The description belongs on what takes focus: the single child when it's an element, else
  // this wrapper, added to any description the child already has. A controlled tooltip's caller
  // describes its own target.
  const describedBy = open && !controlled ? tipId : undefined;
  const describable = isValidElement<{ "aria-describedby"?: string }>(children);
  const trigger = describable
    ? cloneElement(children, {
        "aria-describedby": [children.props["aria-describedby"], describedBy].filter(Boolean).join(" ") || undefined,
      })
    : children;

  return (
    <span
      ref={triggerRef}
      className="inline-flex"
      aria-describedby={describable ? undefined : describedBy}
      // A tap fires emulated mouseenter and focus but never the leave, stranding the
      // tooltip over whatever opens next; a touch-started interaction shows nothing.
      onPointerDown={(e) => {
        touchRef.current = e.pointerType !== "mouse";
        // A touch entry event can open it before this lands; a tap never shows it. A controlled
        // tooltip is its caller's to close: hiding here would only clear its position.
        if (touchRef.current && !controlled) hide();
      }}
      onPointerEnter={(e) => {
        if (e.pointerType === "mouse") touchRef.current = false;
      }}
      onMouseEnter={() => !controlled && !touchRef.current && show()}
      onMouseLeave={() => !controlled && hide()}
      onFocus={() => !controlled && !touchRef.current && show()}
      onBlur={() => !controlled && hide()}
    >
      {trigger}
      {open &&
        createPortal(
          <span
            ref={tipRef}
            id={tipId}
            role="tooltip"
            style={{ left: pos?.x ?? 0, top: pos?.y ?? 0, visibility: pos ? "visible" : "hidden" }}
            className={`pointer-events-none fixed z-50 px-2 py-1 rounded bg-surface-950 border border-surface-700 text-[11px] text-text-secondary ${
              multiline ? "max-w-xs whitespace-normal" : "whitespace-nowrap"
            }`}
          >
            {text}
            {detail && <span className="block text-text-subtle">{detail}</span>}
          </span>,
          document.body,
        )}
    </span>
  );
}
