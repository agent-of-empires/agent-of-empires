import { useCallback, useEffect, useRef } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";

const LONG_PRESS_DELAY = 300;
const REPEAT_INTERVAL = 100;
const HORIZONTAL_THRESHOLD = 16;

export type DragAxis = "vertical" | "horizontal-left" | "horizontal-right";

interface Handlers {
  onPointerDown: (e: ReactPointerEvent) => void;
  onPointerMove: (e: ReactPointerEvent) => void;
  onPointerUp: (e: ReactPointerEvent) => void;
  onPointerCancel: (e: ReactPointerEvent) => void;
  onPointerLeave: (e: ReactPointerEvent) => void;
}

// Press to tap (fires once on release); press and hold to repeat the same
// vertical arrow; drag horizontally mid-press to emit horizontal arrows
// instead. Dominant axis wins on diagonal drags. Emits an "axis change"
// callback so callers can show a visual hint.
export function useLongPressDrag(opts: {
  onRepeat: () => void;
  onHorizontal: (direction: "left" | "right") => void;
  onAxisChange?: (axis: DragAxis) => void;
}): Handlers {
  const { onRepeat, onHorizontal, onAxisChange } = opts;
  const delayTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const intervalTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const startX = useRef(0);
  const startY = useRef(0);
  const axis = useRef<DragAxis>("vertical");
  const pressed = useRef(false);
  const emitted = useRef(false); // true once any emit fired this press

  const clearTimers = useCallback(() => {
    if (delayTimer.current) {
      clearTimeout(delayTimer.current);
      delayTimer.current = null;
    }
    if (intervalTimer.current) {
      clearInterval(intervalTimer.current);
      intervalTimer.current = null;
    }
  }, []);

  useEffect(() => clearTimers, [clearTimers]);

  const startInterval = useCallback(() => {
    if (intervalTimer.current) return;
    intervalTimer.current = setInterval(() => {
      emitted.current = true;
      if (axis.current === "vertical") {
        onRepeat();
      } else if (axis.current === "horizontal-left") {
        onHorizontal("left");
      } else {
        onHorizontal("right");
      }
    }, REPEAT_INTERVAL);
  }, [onRepeat, onHorizontal]);

  const onPointerDown = useCallback(
    (e: ReactPointerEvent) => {
      pressed.current = true;
      emitted.current = false;
      startX.current = e.clientX;
      startY.current = e.clientY;
      axis.current = "vertical";
      onAxisChange?.("vertical");
      clearTimers();
      delayTimer.current = setTimeout(() => {
        if (pressed.current) startInterval();
      }, LONG_PRESS_DELAY);
    },
    [clearTimers, startInterval, onAxisChange],
  );

  const onPointerMove = useCallback(
    (e: ReactPointerEvent) => {
      if (!pressed.current) return;
      const dx = e.clientX - startX.current;
      const dy = e.clientY - startY.current;
      // Dominant axis wins on diagonal drags.
      const horizontal =
        Math.abs(dx) > Math.abs(dy) && Math.abs(dx) > HORIZONTAL_THRESHOLD;
      const next: DragAxis = horizontal
        ? dx > 0
          ? "horizontal-right"
          : "horizontal-left"
        : "vertical";
      if (next !== axis.current) {
        axis.current = next;
        onAxisChange?.(next);
      }
    },
    [onAxisChange],
  );

  // Short press + release with no horizontal drag and no interval emits
  // fires a single "tap" — the same effect as one repeat. Long-press that
  // triggered the interval suppresses the tap.
  const onPointerUp = useCallback(() => {
    if (pressed.current && !emitted.current && axis.current === "vertical") {
      onRepeat();
    }
    pressed.current = false;
    clearTimers();
    axis.current = "vertical";
    onAxisChange?.("vertical");
  }, [clearTimers, onAxisChange, onRepeat]);

  const cancel = useCallback(() => {
    pressed.current = false;
    clearTimers();
    axis.current = "vertical";
    onAxisChange?.("vertical");
  }, [clearTimers, onAxisChange]);

  return {
    onPointerDown,
    onPointerMove,
    onPointerUp,
    onPointerCancel: cancel,
    onPointerLeave: cancel,
  };
}
