import { useCallback, useEffect, useRef } from "react";
import type { MouseEvent as ReactMouseEvent } from "react";

export const HOLD_REPEAT_DELAY_MS = 400;
export const HOLD_REPEAT_INTERVAL_MS = 80;

/**
 * A tap fires once on click; holding fires after `HOLD_REPEAT_DELAY_MS` and then every
 * `HOLD_REPEAT_INTERVAL_MS` until release. Tapping on click rather than pointerdown keeps a
 * swipe that starts on the key (the browser cancels the pointer) from sending anything.
 */
export function useHoldRepeat(fire: () => void) {
  const fireRef = useRef(fire);
  useEffect(() => {
    fireRef.current = fire;
  }, [fire]);
  const delayRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  // Set once a hold has fired, so the click that ends it does not fire again.
  const heldRef = useRef(false);

  const stop = useCallback(() => {
    if (delayRef.current) clearTimeout(delayRef.current);
    if (intervalRef.current) clearInterval(intervalRef.current);
    delayRef.current = null;
    intervalRef.current = null;
  }, []);
  useEffect(() => stop, [stop]);

  const onPointerDown = useCallback(() => {
    stop();
    heldRef.current = false;
    delayRef.current = setTimeout(() => {
      heldRef.current = true;
      fireRef.current();
      intervalRef.current = setInterval(() => fireRef.current(), HOLD_REPEAT_INTERVAL_MS);
    }, HOLD_REPEAT_DELAY_MS);
  }, [stop]);

  const onClick = useCallback((e: ReactMouseEvent) => {
    e.preventDefault();
    if (heldRef.current) heldRef.current = false;
    else fireRef.current();
  }, []);

  return {
    onPointerDown,
    onPointerUp: stop,
    // No click follows a cancel, so the next keyboard activation must not be swallowed.
    onPointerCancel: () => {
      stop();
      heldRef.current = false;
    },
    onPointerLeave: stop,
    onClick,
    // A long press must not open the iOS callout or the context menu.
    onContextMenu: (e: ReactMouseEvent) => e.preventDefault(),
  };
}
