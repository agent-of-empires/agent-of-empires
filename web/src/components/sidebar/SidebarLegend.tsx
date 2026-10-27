import { useRef, useSyncExternalStore, type ReactNode } from "react";
import { Pin } from "lucide-react";
import { ContextMenu } from "../ContextMenu";
import { StatusGlyph } from "../StatusGlyph";
import { statusLegend, type StatusLegendRow, type StatusMeaning } from "../../lib/statusLegend";
import { useContextMenu } from "../useContextMenu";
import { MARK_MEANING } from "../../lib/sidebarMarks";
import { STATUS_DOT_CLASS } from "../../lib/session";
import { TOOLBAR_BUTTON, TOOLBAR_TINT } from "./styles";

const REDUCED_MOTION = "(prefers-reduced-motion: reduce)";

function subscribeReducedMotion(onChange: () => void) {
  const query = window.matchMedia?.(REDUCED_MOTION);
  query?.addEventListener?.("change", onChange);
  return () => query?.removeEventListener?.("change", onChange);
}

function prefersReducedMotion() {
  return !!window.matchMedia?.(REDUCED_MOTION).matches;
}

function LegendGlyph({ row, animate }: { row: StatusLegendRow; animate: boolean }) {
  // A fresh-idle glyph needs an Idle that just started; any status draws Idle's glyph for Dormant.
  const status = row.statuses[0] ?? "Idle";
  return (
    <StatusGlyph
      status={status}
      createdAt={null}
      idleEnteredAt={row.variant === "fresh" ? new Date().toISOString() : null}
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

/** The sidebar header's "?": what the status glyphs and marks mean. A popover, or a sheet on a phone. */
export function SidebarLegend() {
  const buttonRef = useRef<HTMLButtonElement>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<{ x: number; y: number }>();
  const reducedMotion = useSyncExternalStore(subscribeReducedMotion, prefersReducedMotion, () => false);

  const close = () => {
    closeMenu();
    buttonRef.current?.focus();
  };
  const toggle = () => {
    if (menu) return close();
    const rect = buttonRef.current?.getBoundingClientRect();
    openMenu({ x: rect?.left ?? 0, y: (rect?.bottom ?? 0) + 4 });
    // The popover isn't modal, so it takes focus itself; Escape then reaches it. The phone sheet is
    // modal and has already focused its close button, where its Tab trap starts; leave that be.
    requestAnimationFrame(() => {
      if (!menuRef.current?.contains(document.activeElement)) bodyRef.current?.focus({ preventScroll: true });
    });
  };

  return (
    <>
      <button
        ref={buttonRef}
        type="button"
        onClick={toggle}
        aria-label="What do these symbols mean?"
        aria-haspopup="dialog"
        aria-expanded={!!menu}
        data-testid="sidebar-legend-toggle"
        className={`${TOOLBAR_BUTTON} ${TOOLBAR_TINT(!!menu)} text-sm font-semibold`}
      >
        ?
      </button>
      {menu && (
        <ContextMenu
          menu={menu}
          menuRef={menuRef}
          testId="sidebar-legend"
          minWidth="w-[340px] max-w-[calc(100vw-16px)] max-md:max-w-none"
          sheetOnMobile
          label="What the sidebar symbols mean"
          heading={<span className="text-xs font-medium uppercase tracking-wide text-text-subtle">Symbols</span>}
          onClose={close}
          returnFocusTo={buttonRef}
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
            <ul className="mt-1 border-t border-surface-700/60 pt-1">
              <Row mark={<span className="text-status-unread">●</span>} meaning={MARK_MEANING.unread} />
              <Row mark={<Pin className="inline h-3 w-3 -rotate-45 text-brand-400" />} meaning={MARK_MEANING.pinned} />
              <Row mark={<span className="text-favorite">✦</span>} meaning={MARK_MEANING.favorite} />
              <Row mark={<span className="text-[10px] text-text-dim">◆</span>} meaning={MARK_MEANING.pinnedProject} />
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
