import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { clampMenuPosition } from "../lib/menuPosition";

// Shared hover tooltip used by the sidebar control row (grouping axis, filter,
// new session) and the sort picker. Renders a custom styled span instead of
// the native browser `title` so every control shares one look. Lives in its
// own module so SidebarSortPicker can import it without a cycle back through
// WorkspaceSidebar (which imports SidebarSortPicker).
//
// The popup is portaled to document.body and positioned `fixed`, so it escapes
// any `overflow` ancestor that would otherwise clip it. The sidebar trigger
// rows live inside an `overflow-x-hidden overflow-y-auto` scroller, so a span
// nested under the trigger (the old approach) got cut off at the sidebar edge
// regardless of z-index. See #2214.
export function Tooltip({
  text,
  children,
  multiline = false,
}: {
  text: string;
  children: ReactNode;
  // Single-line callers (sidebar, sort picker) keep the default `whitespace-nowrap`
  // pill. Set `multiline` for a sentence-length explanation that should wrap inside
  // a width cap instead of stretching off-screen.
  multiline?: boolean;
}) {
  const triggerRef = useRef<HTMLSpanElement>(null);
  const tipRef = useRef<HTMLSpanElement>(null);
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null);

  const show = () => {
    setPos(null);
    setOpen(true);
  };
  const hide = () => {
    setOpen(false);
    setPos(null);
  };

  // Measure the trigger and the tooltip after mount, center the tooltip below
  // the trigger, and clamp it inside the viewport. Runs before paint so the
  // tooltip never flashes at an unclamped spot; it stays `visibility: hidden`
  // until `pos` is set.
  useLayoutEffect(() => {
    if (!open || !triggerRef.current || !tipRef.current) return;
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
  }, [open, text]);

  // A fixed tooltip detaches from its trigger when an ancestor scrolls or the
  // window resizes; dismiss it rather than tracking the moving anchor. Capture
  // phase is needed because scroll events on the sidebar scroller do not bubble.
  useEffect(() => {
    if (!open) return;
    window.addEventListener("scroll", hide, true);
    window.addEventListener("resize", hide);
    return () => {
      window.removeEventListener("scroll", hide, true);
      window.removeEventListener("resize", hide);
    };
  }, [open]);

  return (
    <span ref={triggerRef} className="inline-flex" onMouseEnter={show} onMouseLeave={hide} onFocus={show} onBlur={hide}>
      {children}
      {open &&
        createPortal(
          <span
            ref={tipRef}
            role="tooltip"
            style={{ left: pos?.x ?? 0, top: pos?.y ?? 0, visibility: pos ? "visible" : "hidden" }}
            className={`pointer-events-none fixed z-50 px-2 py-1 rounded bg-surface-950 border border-surface-700 text-[11px] text-text-secondary ${
              multiline ? "max-w-xs whitespace-normal" : "whitespace-nowrap"
            }`}
          >
            {text}
          </span>,
          document.body,
        )}
    </span>
  );
}
