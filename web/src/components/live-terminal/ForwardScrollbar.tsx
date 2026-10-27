import { useCallback, useEffect, useRef, useState } from "react";
import type { KeyboardEvent, PointerEvent as ReactPointerEvent, ReactNode } from "react";
import { useHoldRepeat } from "../../hooks/useHoldRepeat";
import { FORWARD_SCROLLBAR_WIDTH_PX, forwardScrollRate } from "../../lib/forwardScrollbar";
import { StrokeIcon } from "../icons";

const THUMB_PX = 28;
/** A late frame must not turn into a burst of notches. */
const MAX_FRAME_MS = 64;

const HOLD_BUTTON = "w-full text-text-muted hover:text-text-primary hover:bg-surface-800 transition-colors";

function HoldButton({
  label,
  fire,
  className,
  tabIndex,
  children,
}: {
  label: string;
  fire: () => void;
  className: string;
  tabIndex?: number;
  children?: ReactNode;
}) {
  const hold = useHoldRepeat(fire);
  return (
    <button type="button" aria-label={label} tabIndex={tabIndex} className={className} {...hold}>
      {children}
    </button>
  );
}

interface Drag {
  pointerId: number;
  originY: number;
  travel: number;
  offset: number;
  acc: number;
  last: number;
  raf: number;
}

/**
 * Scroll controls beside a full-screen mouse app. AoE cannot know where the app is in its transcript, so the thumb
 * is a spring-centred rate control, not a position; it exposes no scrollbar semantics. Notches are negative to
 * scroll up.
 */
export function ForwardScrollbar({
  disabled,
  pageNotches,
  scrollNotches,
  cancelScroll,
}: {
  disabled: boolean;
  pageNotches: () => number;
  scrollNotches: (notches: number) => void;
  cancelScroll: () => void;
}) {
  const trackRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef<Drag | null>(null);
  const [offset, setOffset] = useState<number | null>(null);
  const scrollRef = useRef(scrollNotches);
  useEffect(() => {
    scrollRef.current = scrollNotches;
  }, [scrollNotches]);

  const step = (notches: number) => {
    if (!disabled) scrollNotches(notches);
  };

  const stopDrag = useCallback(() => {
    const drag = dragRef.current;
    if (!drag) return;
    cancelAnimationFrame(drag.raf);
    dragRef.current = null;
    setOffset(null);
    cancelScroll();
  }, [cancelScroll]);
  useEffect(() => stopDrag, [stopDrag]);

  const onThumbDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (disabled || dragRef.current || e.button !== 0) return;
    e.currentTarget.setPointerCapture?.(e.pointerId);
    const travel = Math.max(0, ((trackRef.current?.getBoundingClientRect().height ?? 0) - THUMB_PX) / 2);
    const loop = (now: number) => {
      const drag = dragRef.current;
      if (!drag) return;
      drag.acc += (forwardScrollRate(drag.offset, drag.travel) * Math.min(MAX_FRAME_MS, now - drag.last)) / 1000;
      drag.last = now;
      const whole = Math.trunc(drag.acc);
      if (whole !== 0) {
        drag.acc -= whole;
        scrollRef.current(whole);
      }
      drag.raf = requestAnimationFrame(loop);
    };
    dragRef.current = {
      pointerId: e.pointerId,
      originY: e.clientY,
      travel,
      offset: 0,
      acc: 0,
      last: performance.now(),
      raf: requestAnimationFrame(loop),
    };
    setOffset(0);
  };
  const onThumbMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (!drag || e.pointerId !== drag.pointerId) return;
    drag.offset = Math.max(-drag.travel, Math.min(drag.travel, e.clientY - drag.originY));
    setOffset(drag.offset);
  };
  const onThumbEnd = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.pointerId === dragRef.current?.pointerId) stopDrag();
  };
  const onThumbKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const notches: Record<string, number> = {
      ArrowUp: -1,
      ArrowDown: 1,
      PageUp: -pageNotches(),
      PageDown: pageNotches(),
    };
    const n = notches[e.key];
    if (n === undefined) return;
    e.preventDefault();
    step(n);
  };

  return (
    <div
      role="group"
      aria-label="Scroll controls"
      data-forward-scrollbar
      className="absolute top-0 bottom-[8px] right-[6px] flex flex-col select-none bg-surface-900 border-l border-surface-700"
      style={{ width: FORWARD_SCROLLBAR_WIDTH_PX, touchAction: "none" }}
      // Keep focus on the terminal input.
      onMouseDown={(e) => e.preventDefault()}
      onContextMenu={(e) => e.preventDefault()}
    >
      <HoldButton
        label="Scroll up"
        fire={() => step(-1)}
        className={`${HOLD_BUTTON} h-[18px] shrink-0 flex items-center justify-center`}
      >
        <StrokeIcon size={12} strokeWidth="2" hidden>
          <polyline points="6 15 12 9 18 15" />
        </StrokeIcon>
      </HoldButton>
      <div ref={trackRef} className="relative flex-1 min-h-0">
        <HoldButton
          label="Page up"
          tabIndex={-1}
          fire={() => step(-pageNotches())}
          className={`${HOLD_BUTTON} absolute inset-x-0 top-0 bottom-1/2`}
        />
        <HoldButton
          label="Page down"
          tabIndex={-1}
          fire={() => step(pageNotches())}
          className={`${HOLD_BUTTON} absolute inset-x-0 top-1/2 bottom-0`}
        />
        <div
          role="button"
          tabIndex={0}
          aria-label="Drag to scroll"
          className={`absolute inset-x-[2px] top-1/2 rounded-md ${offset === null ? "bg-surface-600" : "bg-brand-500"}`}
          style={{ height: THUMB_PX, transform: `translateY(calc(-50% + ${offset ?? 0}px))` }}
          onPointerDown={onThumbDown}
          onPointerMove={onThumbMove}
          onPointerUp={onThumbEnd}
          onPointerCancel={onThumbEnd}
          onLostPointerCapture={onThumbEnd}
          onKeyDown={onThumbKey}
        />
      </div>
      <HoldButton
        label="Scroll down"
        fire={() => step(1)}
        className={`${HOLD_BUTTON} h-[18px] shrink-0 flex items-center justify-center`}
      >
        <StrokeIcon size={12} strokeWidth="2" hidden>
          <polyline points="6 9 12 15 18 9" />
        </StrokeIcon>
      </HoldButton>
    </div>
  );
}
