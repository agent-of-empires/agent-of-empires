import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import { wheelNotches } from "../../lib/liveMouse";
import type { LiveFrame } from "../../hooks/useLiveTerminal";
import { MAX_QUEUED_TOUCH_NOTCHES, NotchPacer } from "./pacing";

/** Pane lines per line-height of finger travel; small, because each notch waits on a remote redraw. */
export const FORWARD_TOUCH_GAIN = 1.25;
/** Per-ms momentum decay; stops sooner than a native scroller since every notch redraws a remote app. */
const MOMENTUM_DECAY_PER_MS = 0.992;
const MOMENTUM_STOP_VELOCITY = 0.05;

type Cell = { col: number; row: number };

/** Wheel, paced touch-notch, momentum, and mouse-button forwarding to a full-screen mouse app. */
export function useForwardInput({
  streamFrame,
  lineH,
  rowsRef,
  forwardModeRef,
  mouseSgrRef,
  pointerCell,
  inputPaneMiddleRow,
  forwardWheel,
  forwardButton,
  inputRef,
  armAgentClipboard,
}: {
  streamFrame: LiveFrame | null;
  lineH: number;
  rowsRef: RefObject<number>;
  forwardModeRef: RefObject<boolean>;
  mouseSgrRef: RefObject<boolean>;
  pointerCell: (clientX: number, clientY: number) => Cell;
  inputPaneMiddleRow: () => number;
  forwardWheel: (up: boolean, sgr: boolean, col: number, row: number) => void;
  forwardButton: (
    baseButton: number,
    release: boolean,
    motion: boolean,
    sgr: boolean,
    col: number,
    row: number,
  ) => void;
  inputRef: RefObject<HTMLTextAreaElement | null>;
  armAgentClipboard?: () => void;
}) {
  const wheelAccumRef = useRef(0);
  const [notchPacer] = useState(() => new NotchPacer());
  // The pressed button of a forwarded mouse drag and its last reported cell (one motion report per cell).
  const forwardBtnRef = useRef<number | null>(null);
  const lastForwardCellRef = useRef<{ col: number; row: number } | null>(null);
  const onWheel = useCallback(
    (e: React.WheelEvent) => {
      if (!forwardModeRef.current) return;
      const unit = lineH || 16;
      const factor = e.deltaMode === 1 ? unit : e.deltaMode === 2 ? unit * (rowsRef.current || 1) : 1;
      wheelAccumRef.current += e.deltaY * factor;
      const { notches, remainder } = wheelNotches(wheelAccumRef.current, unit, 8);
      wheelAccumRef.current = remainder;
      if (notches === 0) return;
      const { col, row } = pointerCell(e.clientX, e.clientY);
      for (let i = 0; i < Math.abs(notches); i++) forwardWheel(notches < 0, mouseSgrRef.current, col, row);
    },
    [lineH, rowsRef, pointerCell, forwardWheel, forwardModeRef, mouseSgrRef],
  );

  const cancelTouchWheelQueue = useCallback(() => notchPacer.cancel(), [notchPacer]);
  const enqueueTouchWheelDelta = useCallback(
    (deltaPx: number, clientX: number, clientY: number) => {
      wheelAccumRef.current += deltaPx;
      const { notches, remainder } = wheelNotches(wheelAccumRef.current, lineH || 16, MAX_QUEUED_TOUCH_NOTCHES);
      wheelAccumRef.current = remainder;
      if (notches === 0) return;
      notchPacer.enqueue(notches, (up, count) => {
        if (!forwardModeRef.current) return;
        const { col } = pointerCell(clientX, clientY);
        const row = inputPaneMiddleRow();
        for (let i = 0; i < count; i++) forwardWheel(up, mouseSgrRef.current, col, row);
      });
    },
    [lineH, notchPacer, pointerCell, forwardWheel, forwardModeRef, mouseSgrRef, inputPaneMiddleRow],
  );
  useEffect(() => cancelTouchWheelQueue, [cancelTouchWheelQueue]);
  // A frame after a forwarded notch acknowledges it, releasing the next burst.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (streamFrame) notchPacer.onFrame();
  }, [streamFrame, notchPacer]);

  // Forward mode has no native scroller, so flick inertia is synthesized and coasts on lift.
  const momentumRef = useRef<{ v: number; lastT: number; x: number; y: number; raf: number } | null>(null);
  const stopMomentum = useCallback(() => {
    if (momentumRef.current) cancelAnimationFrame(momentumRef.current.raf);
    momentumRef.current = null;
  }, []);
  useEffect(() => stopMomentum, [stopMomentum]);
  const startMomentum = useCallback(
    (velocity: number, clientX: number, clientY: number) => {
      stopMomentum();
      const state = { v: velocity, lastT: performance.now(), x: clientX, y: clientY, raf: 0 };
      momentumRef.current = state;
      const step = (now: number) => {
        if (momentumRef.current !== state) return;
        if (!forwardModeRef.current) {
          momentumRef.current = null;
          return;
        }
        // Clamped so one late frame cannot teleport the transcript.
        const dt = Math.min(64, Math.max(0, now - state.lastT));
        state.lastT = now;
        enqueueTouchWheelDelta(-state.v * dt * FORWARD_TOUCH_GAIN, state.x, state.y);
        state.v *= Math.pow(MOMENTUM_DECAY_PER_MS, dt);
        if (Math.abs(state.v) < MOMENTUM_STOP_VELOCITY) momentumRef.current = null;
        else state.raf = requestAnimationFrame(step);
      };
      state.raf = requestAnimationFrame(step);
    },
    [stopMomentum, enqueueTouchWheelDelta, forwardModeRef],
  );
  // Mouse buttons for a full-screen mouse app; touch has its own path and Shift keeps local selection.
  const onPointerDown = useCallback(
    (e: React.PointerEvent) => {
      if (e.pointerType !== "mouse" || !forwardModeRef.current || e.shiftKey) return;
      const base = [0, 1, 2].includes(e.button) ? e.button : -1;
      // A primary press on a link belongs to the browser; capture would retarget the click away from it.
      if (base < 0 || (base === 0 && (e.target as Element | null)?.closest?.("a[href]"))) return;
      e.preventDefault();
      inputRef.current?.focus();
      const { col, row } = pointerCell(e.clientX, e.clientY);
      forwardButton(base, false, false, mouseSgrRef.current, col, row);
      forwardBtnRef.current = base;
      lastForwardCellRef.current = { col, row };
      try {
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      } catch {
        // Unsupported in jsdom; capture is optional.
      }
    },
    [pointerCell, forwardButton, inputRef, forwardModeRef, mouseSgrRef],
  );
  const onPointerMove = useCallback(
    (e: React.PointerEvent) => {
      if (e.pointerType !== "mouse" || forwardBtnRef.current == null) return;
      const { col, row } = pointerCell(e.clientX, e.clientY);
      const last = lastForwardCellRef.current;
      if (last && last.col === col && last.row === row) return;
      e.preventDefault();
      lastForwardCellRef.current = { col, row };
      forwardButton(forwardBtnRef.current, false, true, mouseSgrRef.current, col, row);
    },
    [pointerCell, forwardButton, mouseSgrRef],
  );
  const endPointerForward = useCallback(
    (e: React.PointerEvent) => {
      if (e.pointerType !== "mouse" || forwardBtnRef.current == null) return;
      e.preventDefault();
      const { col, row } = pointerCell(e.clientX, e.clientY);
      const button = forwardBtnRef.current;
      // Agents emit OSC 52 after the release ending a selection; arm while this is still a user gesture.
      if (button === 0) armAgentClipboard?.();
      forwardButton(button, true, false, mouseSgrRef.current, col, row);
      forwardBtnRef.current = null;
      lastForwardCellRef.current = null;
      try {
        (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
      } catch {
        // Capture may never have been taken.
      }
    },
    [pointerCell, forwardButton, armAgentClipboard, mouseSgrRef],
  );

  return {
    wheelAccumRef,
    onWheel,
    enqueueTouchWheelDelta,
    cancelTouchWheelQueue,
    startMomentum,
    stopMomentum,
    onPointerDown,
    onPointerMove,
    endPointerForward,
  };
}
