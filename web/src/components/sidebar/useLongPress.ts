import { useEffect, useRef } from "react";
import { exceedsTouchSlop } from "../../lib/longPress";

/** Opens after a 500ms hold unless the finger moves past the touch slop. */
export function useLongPress(enabled: boolean, onLongPress: (x: number, y: number) => void) {
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const fired = useRef(false);
  const start = useRef<{ x: number; y: number } | null>(null);

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  const clear = () => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
  };

  const handlers = {
    onTouchStart: (e: React.TouchEvent) => {
      clear();
      fired.current = false;
      if (!enabled) return;
      const touch = e.touches[0];
      if (!touch) return;
      const x = touch.clientX;
      const y = touch.clientY;
      start.current = { x, y };
      timer.current = setTimeout(() => {
        fired.current = true;
        onLongPress(x, y);
      }, 500);
    },
    onTouchMove: (e: React.TouchEvent) => {
      const touch = e.touches[0];
      if (!touch || !start.current) return;
      if (exceedsTouchSlop(start.current, { x: touch.clientX, y: touch.clientY })) clear();
    },
    onTouchEnd: (e: React.TouchEvent) => {
      clear();
      if (fired.current) e.preventDefault();
    },
    onTouchCancel: clear,
  };

  return { handlers, fired };
}
