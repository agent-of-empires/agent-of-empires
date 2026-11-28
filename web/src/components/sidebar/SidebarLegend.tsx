import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { Pin } from "lucide-react";
import { ContextMenu } from "../ContextMenu";
import { StatusGlyph } from "../StatusGlyph";
import { statusLegend, type StatusLegendRow, type StatusMeaning } from "../../lib/statusLegend";
import { useContextMenu } from "../useContextMenu";
import { MARK_MEANING } from "../../lib/sidebarMarks";
import { STATUS_DOT_CLASS } from "../../lib/session";

const REDUCED_MOTION = "(prefers-reduced-motion: reduce)";
const INTERACTIVE = 'a[href], button, input, select, textarea, [contenteditable], [tabindex]:not([tabindex="-1"])';

function subscribeReducedMotion(onChange: () => void) {
  const query = window.matchMedia?.(REDUCED_MOTION);
  query?.addEventListener?.("change", onChange);
  return () => query?.removeEventListener?.("change", onChange);
}

function prefersReducedMotion() {
  return !!window.matchMedia?.(REDUCED_MOTION).matches;
}

function LegendGlyph({ row, animate }: { row: StatusLegendRow; animate: boolean }) {
  // The fresh and Dormant rows draw Idle's glyph in their variant.
  const status = row.statuses[0] ?? "Idle";
  return (
    <StatusGlyph
      status={status}
      createdAt={null}
      fresh={row.variant === "fresh"}
      dormant={row.variant === "dormant"}
      animate={animate}
    />
  );
}

function Row({ mark, meaning, testId }: { mark: ReactNode; meaning: StatusMeaning; testId?: string }) {
  return (
    <li className="flex items-baseline gap-2.5 py-1" data-testid={testId}>
      <span className="w-4 shrink-0 text-center font-mono text-sm leading-none" aria-hidden="true">
        {mark}
      </span>
      <span className="min-w-0 text-[13px] text-text-primary">
        {meaning.label}
        <span className="text-text-subtle"> · {meaning.description}</span>
      </span>
    </li>
  );
}

/**
 * What the sidebar's glyphs and marks mean, opened from the app's ⋯ menu: a popover at `anchor` on desktop,
 * a sheet on a phone.
 */
export function SidebarLegend({
  anchor,
  onClose,
  opener,
}: {
  anchor: { x: number; y: number };
  onClose: () => void;
  /** The control that opened it, where focus goes back. */
  opener: HTMLElement | null;
}) {
  const returnFocusTo = useRef(opener);
  const bodyRef = useRef<HTMLDivElement>(null);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<{ x: number; y: number }>();
  const reducedMotion = useSyncExternalStore(subscribeReducedMotion, prefersReducedMotion, () => false);
  const [initialAnchor] = useState(anchor);
  useEffect(() => openMenu(initialAnchor), [initialAnchor, openMenu]);

  const close = () => {
    closeMenu();
    returnFocusTo.current?.focus();
  };
  // An outside click closes the popover without `close`: back to the opener when the click landed on
  // nothing focusable, left alone when it was a control. Safari doesn't focus a clicked button, so
  // the pointer's target decides, not only where focus went.
  const wasOpen = useRef(false);
  const pressedControl = useRef(false);
  useEffect(() => {
    if (menu) {
      // The popover isn't modal, so it takes focus itself once shown; Escape then reaches it. The phone sheet
      // is modal and has already focused its close button, where its Tab trap starts; leave that be.
      if (!wasOpen.current && !menuRef.current?.contains(document.activeElement)) {
        bodyRef.current?.focus({ preventScroll: true });
      }
      wasOpen.current = true;
      pressedControl.current = false;
      const onPointerDown = (e: PointerEvent) => {
        pressedControl.current = !!(e.target as Element | null)?.closest?.(INTERACTIVE);
      };
      document.addEventListener("pointerdown", onPointerDown, true);
      return () => document.removeEventListener("pointerdown", onPointerDown, true);
    }
    if (!wasOpen.current) return;
    wasOpen.current = false;
    const active = document.activeElement;
    if ((!active || active === document.body) && !pressedControl.current) {
      returnFocusTo.current?.focus({ preventScroll: true });
    }
    onClose();
  }, [menu, menuRef, onClose, returnFocusTo]);

  return (
    <>
      {menu && (
        <ContextMenu
          menu={menu}
          menuRef={menuRef}
          testId="sidebar-legend"
          minWidth="w-[340px] max-w-[calc(100vw-16px)] max-md:max-w-none"
          sheetOnMobile
          label="What the sidebar symbols mean"
          heading="Symbols"
          onClose={close}
          returnFocusTo={returnFocusTo}
        >
          <div
            ref={bodyRef}
            tabIndex={-1}
            role="group"
            aria-label="What the sidebar symbols mean"
            className="px-3 pb-2 outline-none"
          >
            <ul>
              {statusLegend().map((row) => (
                <Row
                  key={row.label}
                  mark={
                    <span className={row.colorClass}>
                      <LegendGlyph row={row} animate={!reducedMotion} />
                    </span>
                  }
                  meaning={row}
                  testId="sidebar-legend-status"
                />
              ))}
            </ul>
            <ul className="mt-1 border-t border-surface-700/50 pt-1">
              <Row mark={<span className="text-status-unread">●</span>} meaning={MARK_MEANING.unread} />
              <Row mark={<Pin className="inline h-3 w-3 -rotate-45 text-brand-400" />} meaning={MARK_MEANING.pinned} />
              <Row mark={<span className="text-[10px] text-text-dim">◆</span>} meaning={MARK_MEANING.pinnedProject} />
              <Row mark={<span className="text-[10px] text-text-dim">◇</span>} meaning={MARK_MEANING.project} />
              <Row
                mark={<span className={`inline-block h-2 w-2 rounded-full ${STATUS_DOT_CLASS.Running}`} />}
                meaning={MARK_MEANING.groupDot}
              />
            </ul>
          </div>
        </ContextMenu>
      )}
    </>
  );
}
