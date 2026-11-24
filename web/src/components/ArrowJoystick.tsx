import { useCallback, useEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import {
  JOYSTICK_ARROWS,
  JOYSTICK_FIRST_REPEAT_MS,
  joystickDirection,
  joystickRepeatMs,
  type Direction,
} from "../lib/arrowJoystick";

const THUMB_TRAVEL_PX = 14;

const clampTravel = (v: number) => Math.max(-THUMB_TRAVEL_PX, Math.min(THUMB_TRAVEL_PX, v));

/**
 * Drag from the pad to send arrow keys; holding repeats faster the further the drag. A tap sends nothing. Its
 * touches never reach the page, so a rightward drag cannot open the sidebar.
 */
export function ArrowJoystick({ onArrow }: { onArrow: (sequence: string) => void }) {
  const padRef = useRef<HTMLDivElement>(null);
  const onArrowRef = useRef(onArrow);
  useEffect(() => {
    onArrowRef.current = onArrow;
  }, [onArrow]);
  const originRef = useRef<{ x: number; y: number; pointerId: number; pointerType: string } | null>(null);
  const directionRef = useRef<Direction | null>(null);
  const distanceRef = useRef(0);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [thumb, setThumb] = useState<{ x: number; y: number } | null>(null);

  const stopRepeat = useCallback(() => {
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = null;
  }, []);

  const emit = useCallback((direction: Direction) => {
    navigator.vibrate?.(5);
    onArrowRef.current(JOYSTICK_ARROWS[direction]);
  }, []);

  // Each tick reads the current distance, so dragging further speeds up a repeat already running.
  const startRepeat = useCallback(() => {
    const tick = () => {
      const direction = directionRef.current;
      if (!direction) return;
      emit(direction);
      timerRef.current = setTimeout(tick, joystickRepeatMs(distanceRef.current));
    };
    timerRef.current = setTimeout(tick, JOYSTICK_FIRST_REPEAT_MS);
  }, [emit]);

  const endGesture = useCallback(() => {
    stopRepeat();
    if (!originRef.current) return;
    originRef.current = null;
    directionRef.current = null;
    setThumb(null);
  }, [stopRepeat]);
  useEffect(() => endGesture, [endGesture]);

  // iOS can drop a touch's pointerup and pointercancel, for example when a system gesture takes the finger
  // or the PWA is backgrounded mid-hold, so the window and page lifecycle also end the gesture.
  useEffect(() => {
    const onPointerEnd = (e: PointerEvent) => {
      if (originRef.current?.pointerId === e.pointerId) endGesture();
    };
    // A new primary pointer of the same type means no other such pointer is down, so a held gesture
    // lost its end event, even when the new press reuses its ID. Capture runs before the pad's own
    // pointerdown, which then starts fresh.
    const onPointerStart = (e: PointerEvent) => {
      if (e.isPrimary && e.pointerType === originRef.current?.pointerType) endGesture();
    };
    const onHidden = () => {
      if (document.visibilityState === "hidden") endGesture();
    };
    window.addEventListener("pointerdown", onPointerStart, true);
    window.addEventListener("pointerup", onPointerEnd, true);
    window.addEventListener("pointercancel", onPointerEnd, true);
    window.addEventListener("blur", endGesture);
    window.addEventListener("pagehide", endGesture);
    document.addEventListener("visibilitychange", onHidden);
    return () => {
      window.removeEventListener("pointerdown", onPointerStart, true);
      window.removeEventListener("pointerup", onPointerEnd, true);
      window.removeEventListener("pointercancel", onPointerEnd, true);
      window.removeEventListener("blur", endGesture);
      window.removeEventListener("pagehide", endGesture);
      document.removeEventListener("visibilitychange", onHidden);
    };
  }, [endGesture]);

  // The drawer swipe listens on window, so the pad's touches stop here.
  useEffect(() => {
    const pad = padRef.current;
    if (!pad) return;
    const stop = (e: TouchEvent) => e.stopPropagation();
    const types = ["touchstart", "touchmove", "touchend", "touchcancel"] as const;
    for (const type of types) pad.addEventListener(type, stop, { passive: true });
    return () => {
      for (const type of types) pad.removeEventListener(type, stop);
    };
  }, []);

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (originRef.current) return;
    e.currentTarget.setPointerCapture?.(e.pointerId);
    originRef.current = { x: e.clientX, y: e.clientY, pointerId: e.pointerId, pointerType: e.pointerType };
    directionRef.current = null;
    setThumb({ x: 0, y: 0 });
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const origin = originRef.current;
    if (!origin || e.pointerId !== origin.pointerId) return;
    const dx = e.clientX - origin.x;
    const dy = e.clientY - origin.y;
    setThumb({ x: clampTravel(dx), y: clampTravel(dy) });
    distanceRef.current = Math.max(Math.abs(dx), Math.abs(dy));
    const direction = joystickDirection(dx, dy);
    if (direction === directionRef.current) return;
    stopRepeat();
    directionRef.current = direction;
    if (!direction) return;
    emit(direction);
    startRepeat();
  };

  const end = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (originRef.current?.pointerId === e.pointerId) endGesture();
  };

  return (
    <div
      ref={padRef}
      role="group"
      aria-label="Arrow keys joystick"
      data-arrow-joystick
      className="absolute right-2 bottom-[60px] z-10 w-12 h-12 rounded-full bg-surface-800/90 border border-surface-700/30 shadow-lg backdrop-blur-sm flex items-center justify-center select-none [-webkit-touch-callout:none]"
      // No scroll, pinch, or double-tap zoom may start on the pad.
      style={{ touchAction: "none" }}
      // Keep focus on the terminal input, so the keyboard stays as it was.
      onMouseDown={(e) => e.preventDefault()}
      onContextMenu={(e) => e.preventDefault()}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onLostPointerCapture={end}
    >
      <svg
        aria-hidden="true"
        width="40"
        height="40"
        viewBox="0 0 40 40"
        className="absolute text-text-muted"
        fill="currentColor"
      >
        <path d="M20 3l3 4h-6zM20 37l3-4h-6zM3 20l4-3v6zM37 20l-4-3v6z" />
      </svg>
      <span
        aria-hidden="true"
        className={`w-5 h-5 rounded-full ${thumb ? "bg-brand-500" : "bg-surface-600"}`}
        style={thumb ? { transform: `translate(${thumb.x}px, ${thumb.y}px)` } : undefined}
      />
    </div>
  );
}
