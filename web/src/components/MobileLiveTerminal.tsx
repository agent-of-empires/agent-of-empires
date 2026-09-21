import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, RefObject } from "react";
import type { AnsiSegment } from "../lib/ansi";
import { LineParseCache, wrapLine } from "../lib/liveTermLines";
import { cursorLineIndex, pointerPaneCell } from "../lib/liveMouse";
import type { LiveFrame, LiveStats } from "../hooks/useLiveTerminal";
import { useWebSettings } from "../hooks/useWebSettings";
import { useIsCoarsePointer } from "../hooks/useIsCoarsePointer";
import { useTerminalGestureBoundary } from "../hooks/useTerminalGestureBoundary";
import { useSelectionHold } from "../hooks/useSelectionHold";
import { FrameTimingProbe, mountedBlocks } from "./live-terminal/pacing";
import { FORWARD_TOUCH_GAIN, useForwardInput } from "./live-terminal/useForwardInput";
import { Row } from "./live-terminal/TermRow";
import { LIVE_WINDOW_SCREENS, useLiveEdgeScroll } from "./live-terminal/useLiveEdgeScroll";
import { useTerminalInput } from "./live-terminal/useTerminalInput";
import { StrokeIcon } from "./icons";

// Renders a tmux pane from streamed `capture-pane` frames (src/server/live_ws.rs) as DOM text in a natively
// scrolling container.

const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 28;
const LINE_RATIO = 1.2;
const RESIZE_DEBOUNCE_MS = 150;
/** How long the trimmed row count must stay lower before shrinking; outlasts a stalled stream's spinner gaps. */
const SHRINK_DELAY_MS = 1500;
/** Release velocities (px/ms): below the minimum a drag stops dead; the cap keeps a flick a small continuation. */
const FLICK_MIN_VELOCITY = 0.3;
const FLICK_MAX_VELOCITY = 1.5;
/** A lift this long (ms) after the last move does not coast. */
const FLICK_MAX_PAUSE_MS = 80;
const FLICK_VELOCITY_WINDOW_MS = 100;

// `?livedebug=1` shows the geometry and wire stats the view is working from.
const LIVE_DEBUG = typeof location !== "undefined" && new URLSearchParams(location.search).has("livedebug");

export interface MobileLiveTerminalProps {
  frame: LiveFrame | null;
  /** Wire counters for the `?livedebug=1` overlay. */
  liveStats?: LiveStats;
  transport?: "grid" | "snapshot" | null;
  /** Arms the parent's gesture-bound clipboard write before an agent selection release crosses the socket. */
  armAgentClipboard?: () => void;
  connected: boolean;
  active: boolean;
  /** Off the live edge: the capture window is widened and the jump-to-latest button shows. */
  reading: boolean;
  sendResize: (cols: number, rows: number) => void;
  setWindow: (lines: number) => void;
  setCadence: (fast: boolean) => void;
  enterReading: (rows: number) => void;
  returnToLive: (rows: number) => void;
  /** Returns whether the pane will receive the data. */
  sendData: (data: string) => boolean;
  /** Shared IME word run; `sendData` clears it. */
  typedWordRef: React.RefObject<string>;
  /** Uploads a pasted image and resolves to a path the pane can read, or null. */
  uploadPastedImage: (file: File) => Promise<string | null>;
  forwardWheel: (up: boolean, sgr: boolean, col: number, row: number) => void;
  forwardButton: (
    baseButton: number,
    release: boolean,
    motion: boolean,
    sgr: boolean,
    col: number,
    row: number,
  ) => void;
  /** Virtual Ctrl modifier from the mobile toolbar. */
  ctrlActiveRef: RefObject<boolean>;
  clearCtrl: () => void;
  inputRef: RefObject<HTMLTextAreaElement | null>;
  /** On touch devices input focus means the soft keyboard is up. */
  onInputFocusChange: (focused: boolean) => void;
  /** Bottom-align a short screen chat-style (agents); paired shells top-align like a normal terminal. */
  bottomAlign: boolean;
  /** The soft keyboard occludes the viewport; defers the row latch so keyboard-shrunk rows never reach tmux. */
  keyboardOpen: boolean;
}

/** A frame's rows; `lines` is authoritative when present, and `content` ends with a newline that is not a row. */
function frameLines(frame: LiveFrame): string[] {
  if (frame.lines) return frame.lines;
  const content = frame.content.endsWith("\n") ? frame.content.slice(0, -1) : frame.content;
  return content.split("\n");
}

type FlickSample = { x: number; y: number; t: number };

function pushFlickSample(samples: FlickSample[], sample: FlickSample) {
  samples.push(sample);
  while (samples.length > 1 && sample.t - samples[0]!.t > FLICK_VELOCITY_WINDOW_MS) samples.shift();
}

export function MobileLiveTerminal({
  frame: streamFrame,
  liveStats,
  transport,
  armAgentClipboard,
  connected,
  active,
  reading,
  sendResize,
  setWindow,
  setCadence,
  enterReading,
  returnToLive,
  sendData: sendDataRaw,
  typedWordRef,
  uploadPastedImage,
  forwardWheel,
  forwardButton,
  ctrlActiveRef,
  clearCtrl,
  inputRef,
  onInputFocusChange,
  bottomAlign,
  keyboardOpen,
}: MobileLiveTerminalProps) {
  const { settings, update } = useWebSettings();
  const fontKey = useIsCoarsePointer() ? "mobileFontSize" : "desktopFontSize";
  const configuredFontSize = settings[fontKey];
  // Quotes are stripped so a stray `"` cannot produce an ignored font-family.
  const termFontFamily = (settings.terminalFontFamily ?? "").trim().replace(/"/g, "");
  const fontFamily = termFontFamily ? `"${termFontFamily}", var(--font-mono)` : undefined;
  const [focused, setFocused] = useState(false);
  const [fontSize, setFontSize] = useState(() => configuredFontSize);
  // Adopt a changed setting during render; a pinch drives fontSize live without touching the setting.
  const [lastConfiguredFontSize, setLastConfiguredFontSize] = useState(configuredFontSize);
  if (configuredFontSize !== lastConfiguredFontSize) {
    setLastConfiguredFontSize(configuredFontSize);
    setFontSize(configuredFontSize);
  }
  const scrollerRef = useRef<HTMLDivElement>(null);

  // A selection holds the painted frame.
  const absorbExposedHistory = useCallback(
    (held: LiveFrame | null, next: LiveFrame | null) => {
      if (!reading || !held || !next) return null;
      const heldLines = frameLines(held);
      const nextLines = frameLines(next);
      const older = held.history - heldLines.length - (next.history - nextLines.length);
      // A frame too short for the whole prefix (scrollback cleared mid-selection) would refold forever.
      if (older <= 0 || older > nextLines.length) return null;
      return { ...held, lines: nextLines.slice(0, older).concat(heldLines) };
    },
    [reading],
  );
  const { value: frame, held: selectionHeld } = useSelectionHold(streamFrame, scrollerRef, absorbExposedHistory);

  const lineH = fontSize * LINE_RATIO;
  // Glyph advance measured in the scroller after fonts load, so cols sent to tmux match the real font.
  const measureRef = useRef<HTMLSpanElement>(null);
  const [charW, setCharW] = useState(() => fontSize * 0.6);
  const remeasure = useCallback(() => {
    const w = (measureRef.current?.getBoundingClientRect().width ?? 0) / 20;
    if (w > 0) setCharW((prev) => (Math.abs(prev - w) > 0.01 ? w : prev));
  }, []);
  useLayoutEffect(() => {
    remeasure();
  }, [remeasure, fontSize, fontFamily]);
  useEffect(() => {
    const fonts = (document as Document & { fonts?: { ready: Promise<unknown> } }).fonts;
    // No FontFaceSet in jsdom; the layout-effect measure stands.
    fonts?.ready?.then(() => remeasure()).catch(() => {});
  }, [remeasure]);

  const rowsRef = useRef(0);
  const readingRef = useRef(reading);
  useEffect(() => {
    readingRef.current = reading;
  }, [reading]);
  // Unchanged lines keep their parse and wrap identity across frames, so memoized rows skip them.
  const [parseCache] = useState(() => new LineParseCache());
  const lines = useMemo(() => (frame ? parseCache.lines(frame.lines ?? frame.content) : []), [frame, parseCache]);
  // Columns rendered at; wrapping keeps a frame readable when another client widened the window.
  const [renderCols, setRenderCols] = useState(0);
  const [wrapCache] = useState(() => new WeakMap<AnsiSegment[], { cols: number; rows: AnsiSegment[][] }>());
  const visual = useMemo(() => {
    const cols = renderCols > 0 ? renderCols : Number.POSITIVE_INFINITY;
    const rows: AnsiSegment[][] = [];
    const lineStartRow: number[] = new Array(lines.length);
    const source: Array<{ line: number; wrap: number }> = [];
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i]!;
      let wrapped = wrapCache.get(line);
      if (!wrapped || wrapped.cols !== cols) {
        wrapped = { cols, rows: wrapLine(line, cols) };
        wrapCache.set(line, wrapped);
      }
      lineStartRow[i] = rows.length;
      wrapped.rows.forEach((row, wrap) => {
        rows.push(row);
        source.push({ line: i, wrap });
      });
    }
    return { rows, lineStartRow, source };
  }, [lines, renderCols, wrapCache]);
  const screenRows = frame?.rows ?? 0;
  const spacerLines = Math.max(0, (frame?.history ?? 0) - Math.max(0, lines.length - screenRows));
  // A full-screen mouse app's scrollback is not capturable: no spacer, no native scroll, wheels go to the app.
  const altScreen = frame?.altScreen ?? false;
  const forwardMode = altScreen && (frame?.mouse ?? false);
  const effectiveSpacerLines = forwardMode ? 0 : spacerLines;
  // Gestures yield to a selection so WebKit can drag its handles; the layout keeps `forwardMode` so row keys hold.
  const { forwardModeRef, mouseSgrRef } = useTerminalGestureBoundary({
    scrollerRef,
    forwardMode: forwardMode && !selectionHeld,
    mouseSgr: frame?.mouseSgr ?? false,
  });
  const forwardGestures = forwardMode && !selectionHeld;
  const touchForwardYRef = useRef<number | null>(null);
  // WebKit may synthesize a click after a custom forward-mode drag; a moved touch must not raise the keyboard.
  const touchStartRef = useRef<{ x: number; y: number } | null>(null);
  const suppressTouchClickRef = useRef(false);
  useEffect(() => {
    rowsRef.current = screenRows || rowsRef.current;
  }, [screenRows]);

  const lastNonBlankRow = useMemo(() => {
    for (let i = visual.rows.length - 1; i >= 0; i--) {
      if (visual.rows[i]!.some((s) => s.text.trim() !== "")) return i;
    }
    return -1;
  }, [visual]);

  // Rows to render at the live edge: grows at once, shrinks only after SHRINK_DELAY_MS, so trimming trailing
  // blanks for bottom alignment does not bounce with a spinner on the last row.
  const [renderRowCount, setRenderRowCount] = useState(0);
  const shrinkTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    const target = Math.max(0, lastNonBlankRow + 1);
    setRenderRowCount((current) => {
      if (target >= current) {
        if (shrinkTimerRef.current) clearTimeout(shrinkTimerRef.current);
        shrinkTimerRef.current = null;
        return target;
      }
      shrinkTimerRef.current ??= setTimeout(() => {
        shrinkTimerRef.current = null;
        setRenderRowCount(Math.max(0, lastNonBlankRow + 1));
      }, SHRINK_DELAY_MS);
      return current;
    });
  }, [lastNonBlankRow]);
  useEffect(
    () => () => {
      if (shrinkTimerRef.current) clearTimeout(shrinkTimerRef.current);
    },
    [],
  );

  // Scroller position driving row virtualization.
  const [view, setView] = useState({ top: 0, height: 0 });
  const syncView = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    if (el.scrollLeft !== 0) el.scrollLeft = 0;
    setView((prev) =>
      prev.top === el.scrollTop && prev.height === el.clientHeight
        ? prev
        : { top: el.scrollTop, height: el.clientHeight },
    );
  }, []);

  // The cursor's visual row and column at the live edge, and its pixel top for the keyboard anchor.
  const live = useMemo(() => {
    const none = { row: -1, col: -1, top: null as number | null };
    const cursor = !reading ? (frame?.cursor ?? null) : null;
    if (!cursor) return none;
    const lineIdx = cursorLineIndex(lines.length, screenRows, cursor.y);
    const baseRow = lineIdx < 0 || lineIdx >= lines.length ? -1 : (visual.lineStartRow[lineIdx] ?? -1);
    if (baseRow < 0) return none;
    const cols = renderCols > 0 ? renderCols : Number.POSITIVE_INFINITY;
    const row = baseRow + (Number.isFinite(cols) ? Math.floor(cursor.x / cols) : 0);
    // An agent may park the hardware cursor in blank rows below its UI; its own caret stays visible instead.
    if (row > lastNonBlankRow) return none;
    const col = Number.isFinite(cols) ? cursor.x % cols : cursor.x;
    return { row, col, top: (effectiveSpacerLines + row) * lineH };
  }, [reading, frame, lines.length, screenRows, visual, renderCols, effectiveSpacerLines, lineH, lastNonBlankRow]);

  // One view sync per painted frame; scroll events fire per pixel.
  const viewSyncRafRef = useRef(0);
  const scheduleViewSync = useCallback(() => {
    if (viewSyncRafRef.current !== 0) return;
    viewSyncRafRef.current = requestAnimationFrame(() => {
      viewSyncRafRef.current = 0;
      syncView();
    });
  }, [syncView]);
  useEffect(() => () => cancelAnimationFrame(viewSyncRafRef.current), []);

  const {
    touchActiveRef,
    latchRef,
    cursorAnchorRef,
    liveDetachedRef,
    forceLiveRef,
    liveScrollTarget,
    pinIfWasAtBottom,
    atBottom,
    onScroll,
    jumpToLatest,
  } = useLiveEdgeScroll({
    scrollerRef,
    lineH,
    rowsRef,
    reading,
    enterReading,
    returnToLive,
    forwardModeRef,
    scheduleViewSync,
  });

  // A tap raises the keyboard; focus() must stay synchronous for iOS. A click ending a selection is left alone.
  const focusInputOnTap = useCallback(() => {
    if (suppressTouchClickRef.current) {
      suppressTouchClickRef.current = false;
      return;
    }
    if (document.activeElement === inputRef.current) return;
    const sel = window.getSelection();
    if (sel && !sel.isCollapsed) return;
    inputRef.current?.focus();
  }, [inputRef]);

  // A viewport point as a 1-based pane-0 cell, measured from the (possibly bottom-aligned) rendered content.
  const pointerCell = useCallback(
    (clientX: number, clientY: number) => {
      const el = scrollerRef.current;
      if (!el || charW <= 0 || lineH <= 0) return { col: 1, row: 1 };
      const r = el.getBoundingClientRect();
      const gridTop = el.querySelector<HTMLElement>("[data-live-content]")?.getBoundingClientRect().top ?? r.top;
      const pane0 = frame?.pane0 ?? {
        cols: renderCols > 0 ? renderCols : 1,
        rows: Math.max(1, screenRows || rowsRef.current),
      };
      const compositeCol = Math.floor((clientX - r.left) / charW) + 1;
      const visualRow = Math.floor((clientY - gridTop) / lineH) - effectiveSpacerLines;
      const screenTopVisual = visual.lineStartRow[Math.max(0, lines.length - screenRows)] ?? 0;
      return pointerPaneCell(compositeCol, visualRow - screenTopVisual, pane0);
    },
    [charW, lineH, renderCols, screenRows, effectiveSpacerLines, lines.length, visual, frame?.pane0],
  );
  // Touch wheels report at pane 0's middle row: position-aware apps ignore wheels over their input box.
  const inputPaneMiddleRow = useCallback(
    () => Math.max(1, Math.round((frame?.pane0?.rows ?? rowsRef.current) / 2)),
    [frame?.pane0?.rows],
  );

  const forward = useForwardInput({
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
  });
  const { wheelAccumRef, enqueueTouchWheelDelta, cancelTouchWheelQueue, startMomentum, stopMomentum } = forward;
  // Recent drag samples for the release velocity.
  const flickSamplesRef = useRef<FlickSample[]>([]);
  // Every input path funnels through here, so typing interrupts a coast instead of queueing behind it.
  const sendData = useCallback(
    (data: string) => {
      stopMomentum();
      cancelTouchWheelQueue();
      return sendDataRaw(data);
    },
    [sendDataRaw, stopMomentum, cancelTouchWheelQueue],
  );

  const pinchRef = useRef<{ startDist: number; startSize: number; changed: boolean } | null>(null);
  // Bumped when a font-changing pinch ends, so the resize commits once at gesture end.
  const [pinchGeneration, setPinchGeneration] = useState(0);
  const persistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const touchScrollStartYRef = useRef<number | null>(null);
  const resetTouch = () => {
    touchForwardYRef.current = null;
    touchScrollStartYRef.current = null;
    touchStartRef.current = null;
  };
  const endPinch = () => {
    const changed = pinchRef.current?.changed;
    pinchRef.current = null;
    if (!changed) return;
    setPinchGeneration((g) => g + 1);
    if (persistTimerRef.current) clearTimeout(persistTimerRef.current);
    persistTimerRef.current = setTimeout(() => update({ [fontKey]: fontSize }), 400);
  };
  // Records forward-mode finger travel as queued wheel notches; returns false for a zero move.
  const trackForwardDrag = (touch: { clientX: number; clientY: number }) => {
    const dy = touch.clientY - touchForwardYRef.current!;
    const start = touchStartRef.current;
    if (start && Math.hypot(touch.clientX - start.x, touch.clientY - start.y) > 8) suppressTouchClickRef.current = true;
    touchForwardYRef.current = touch.clientY;
    pushFlickSample(flickSamplesRef.current, { x: touch.clientX, y: touch.clientY, t: performance.now() });
    // Finger down reveals older content, a wheel up, hence the negation.
    enqueueTouchWheelDelta(-dy * FORWARD_TOUCH_GAIN, touch.clientX, touch.clientY);
  };

  const onTouchStart = (e: React.TouchEvent) => {
    touchActiveRef.current = true;
    stopMomentum();
    cancelTouchWheelQueue();
    flickSamplesRef.current = [];
    const [a, b] = [e.touches[0]!, e.touches[1]!];
    if (e.touches.length === 2) {
      pinchRef.current = {
        startDist: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY),
        startSize: fontSize,
        changed: false,
      };
      resetTouch();
      return;
    }
    if (e.touches.length !== 1) return;
    touchStartRef.current = { x: a.clientX, y: a.clientY };
    suppressTouchClickRef.current = false;
    if (forwardModeRef.current) {
      touchForwardYRef.current = a.clientY;
      wheelAccumRef.current = 0;
      flickSamplesRef.current = [{ x: a.clientX, y: a.clientY, t: performance.now() }];
    } else {
      // Detach now: iOS fires touchcancel when promoting the drag to a native scroll, finger still down.
      liveDetachedRef.current = true;
      touchScrollStartYRef.current = a.clientY;
    }
  };
  const onTouchMove = (e: React.TouchEvent) => {
    const t0 = e.touches[0]!;
    if (e.touches.length === 2 && pinchRef.current) {
      e.preventDefault();
      const dist = Math.hypot(t0.clientX - e.touches[1]!.clientX, t0.clientY - e.touches[1]!.clientY);
      const { startDist, startSize } = pinchRef.current;
      if (startDist > 0) {
        const next = Math.round(Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, startSize * (dist / startDist))));
        if (next !== startSize) pinchRef.current.changed = true;
        setFontSize(next);
      }
    } else if (e.touches.length === 1 && forwardModeRef.current && touchForwardYRef.current != null) {
      // Page pan is blocked by touch-action on the scroller; React's touch listeners are passive.
      trackForwardDrag(t0);
    } else if (e.touches.length === 1 && !forwardModeRef.current && touchScrollStartYRef.current != null) {
      // A real scroll (past 8px) enters reading mode at once, so live appends stop sliding rows under the finger.
      if (Math.abs(t0.clientY - touchScrollStartYRef.current) > 8) {
        suppressTouchClickRef.current = true;
        enterReading(rowsRef.current);
      }
    }
  };
  const onTouchEnd = (e: React.TouchEvent) => {
    if (e.touches.length === 0) {
      if (forwardModeRef.current && touchForwardYRef.current != null) {
        // A quick iOS swipe can coalesce every move into touchend.
        const finalTouch = e.changedTouches[0];
        if (finalTouch && finalTouch.clientY !== touchForwardYRef.current) trackForwardDrag(finalTouch);
        const samples = flickSamplesRef.current;
        const first = samples[0];
        const last = samples[samples.length - 1];
        if (first && last && last.t > first.t && performance.now() - last.t <= FLICK_MAX_PAUSE_MS) {
          const raw = (last.y - first.y) / (last.t - first.t);
          const v = Math.max(-FLICK_MAX_VELOCITY, Math.min(FLICK_MAX_VELOCITY, raw));
          if (Math.abs(v) >= FLICK_MIN_VELOCITY) startMomentum(v, last.x, last.y);
        }
      }
      flickSamplesRef.current = [];
      touchActiveRef.current = false;
      resetTouch();
      // Ending at the bottom (a tap, or a scroll back down) re-attaches, undoing touchstart's detach.
      if (atBottom()) {
        liveDetachedRef.current = false;
        returnToLive(rowsRef.current * LIVE_WINDOW_SCREENS);
      }
    }
    if (e.touches.length < 2 && pinchRef.current) endPinch();
  };
  // touchcancel fires with the finger often still down (iOS native scroll promotion): stop tracking, never settle.
  const onTouchCancel = () => {
    touchActiveRef.current = false;
    resetTouch();
    flickSamplesRef.current = [];
    endPinch();
  };
  useEffect(
    () => () => {
      if (persistTimerRef.current) clearTimeout(persistTimerRef.current);
    },
    [],
  );

  // Grid sizing from the latched maximum height; the latch resets when the width changes.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el || !active) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const compute = () => {
      const width = el.clientWidth;
      const height = el.clientHeight;
      if (width <= 0 || height <= 0) return;
      const cols = Math.floor(width / charW);
      const latch = latchRef.current;
      const widthChanged = Math.abs(width - latch.width) > 1;
      // Mid-pinch, or with the keyboard up before a latch exists, only the render width may change.
      if (pinchRef.current || (keyboardOpen && (widthChanged || latch.maxHeight === 0))) {
        if (cols >= 20) setRenderCols(cols);
        return;
      }
      if (widthChanged) {
        latch.width = width;
        latch.maxHeight = height;
      } else if (height > latch.maxHeight) {
        latch.maxHeight = height;
      }
      const rows = Math.floor(latch.maxHeight / lineH);
      // Implausibly small means a hidden or transitioning container.
      if (cols < 20 || rows < 5) return;
      rowsRef.current = rows;
      setRenderCols(cols);
      sendResize(cols, rows);
      if (!readingRef.current) setWindow(rows * LIVE_WINDOW_SCREENS);
    };
    const ro = new ResizeObserver(() => {
      pinIfWasAtBottom();
      if (timer) clearTimeout(timer);
      timer = setTimeout(compute, RESIZE_DEBOUNCE_MS);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [active, charW, lineH, latchRef, sendResize, setWindow, pinIfWasAtBottom, keyboardOpen, pinchGeneration]);

  // Opening the keyboard means typing: return to the live prompt before the viewport shrinks.
  useEffect(() => {
    if (!keyboardOpen && !focused) return;
    const id = requestAnimationFrame(() => {
      const el = scrollerRef.current;
      if (!el) return;
      forceLiveRef.current = true;
      liveDetachedRef.current = false;
      returnToLive(rowsRef.current * LIVE_WINDOW_SCREENS);
      el.scrollTop = liveScrollTarget(el);
      syncView();
    });
    return () => cancelAnimationFrame(id);
  }, [focused, keyboardOpen, forceLiveRef, liveDetachedRef, liveScrollTarget, returnToLive, syncView]);

  // Fast cadence only for the visible, active pane at the live edge.
  useEffect(() => {
    const sync = () => setCadence(active && document.visibilityState === "visible" && !reading);
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, [active, reading, setCadence]);

  const [frameTiming] = useState(() => new FrameTimingProbe());
  useLayoutEffect(() => {
    if (LIVE_DEBUG && streamFrame) frameTiming.record(performance.now(), streamFrame.receivedAt);
  }, [streamFrame, frameTiming]);

  useLayoutEffect(() => {
    if (live.top != null) cursorAnchorRef.current = live.top;
    pinIfWasAtBottom();
    // Match the virtualization window to the pinned position before paint.
    syncView();
    // `renderRowCount` changes the content height at the live edge, which must re-pin.
  }, [lines, spacerLines, lineH, live, renderRowCount, cursorAnchorRef, pinIfWasAtBottom, syncView]);

  const input = useTerminalInput({
    active,
    inputRef,
    typedWordRef,
    ctrlActiveRef,
    clearCtrl,
    sendData,
    uploadPastedImage,
  });

  const cursorRow = connected && !reading ? live.row : -1;
  // Trailing blanks are trimmed only at the normal-screen live edge: reading must keep scrollHeight stable, and
  // a full-screen app owns its whole grid.
  const visibleRowCount = reading || altScreen ? visual.rows.length : Math.min(renderRowCount, visual.rows.length);
  const { blocks, bottomPadLines } = mountedBlocks(visibleRowCount, effectiveSpacerLines, view, lineH);

  return (
    <div className="absolute inset-0" data-live-terminal>
      <div
        ref={scrollerRef}
        onScroll={onScroll}
        onWheel={forward.onWheel}
        onClick={focusInputOnTap}
        onPointerDown={forward.onPointerDown}
        onPointerMove={forward.onPointerMove}
        onPointerUp={forward.endPointerForward}
        onPointerCancel={forward.endPointerForward}
        onContextMenu={(e) => {
          if (forwardModeRef.current) e.preventDefault();
        }}
        onTouchStart={onTouchStart}
        onTouchMove={onTouchMove}
        onTouchEnd={onTouchEnd}
        onTouchCancel={onTouchCancel}
        // A bottom inset, not padding: padding would inflate clientHeight and over-count rows sent to tmux.
        className={`absolute inset-x-0 top-0 bottom-[8px] font-mono flex flex-col ${
          forwardMode ? "overflow-hidden" : "overflow-y-auto overflow-x-clip"
        }`}
        style={
          {
            fontSize: `${fontSize}px`,
            fontFamily,
            lineHeight: `${lineH}px`,
            // A variable, so a re-measure restyles mounted rows without re-rendering them.
            "--term-cell": `${charW}px`,
            background: "var(--term-bg, #1c1c1f)",
            color: "var(--term-fg, #e4e4e7)",
            // A fixed grid: ligatures would merge cells.
            fontVariantLigatures: "none",
            fontFeatureSettings: '"liga" 0, "calt" 0',
            overscrollBehavior: "contain",
            // Declarative, because React's touch listeners are passive and cannot stop the page pan.
            touchAction: forwardGestures ? "none" : undefined,
            // No -webkit-overflow-scrolling: Safari rasterizes that layer at 1x. Browser scroll anchoring is off
            // because the spacer model already keeps the reader's place.
            overflowAnchor: "none",
          } as CSSProperties
        }
      >
        <span
          ref={measureRef}
          aria-hidden="true"
          className="absolute whitespace-pre"
          style={{ visibility: "hidden", pointerEvents: "none" }}
        >
          MMMMMMMMMMMMMMMMMMMM
        </span>
        {/* `mt-auto` bottom-aligns a short screen; it collapses once content overflows. */}
        <div className={`relative whitespace-pre ${bottomAlign ? "mt-auto" : ""}`} data-live-content>
          {blocks.flatMap(({ padLines, start, end }, block) => [
            padLines > 0 ? (
              <div key={`pad-${block}`} style={{ height: `${padLines * lineH}px` }} aria-hidden="true" />
            ) : null,
            // Keyed by pane line (invariant as the agent appends) and wrap offset. Pads sit in the same flat
            // list because a wrapper keyed on the range would remount rows and drop the selection.
            ...visual.rows.slice(start, end).map((segs, j) => {
              const i = start + j;
              const src = visual.source[i]!;
              return (
                <Row
                  key={`${effectiveSpacerLines + src.line}:${src.wrap}`}
                  segs={segs}
                  cursorCol={i === cursorRow ? live.col : null}
                  focused={i === cursorRow && focused}
                />
              );
            }),
          ])}
          {bottomPadLines > 0 && <div style={{ height: `${bottomPadLines * lineH}px` }} aria-hidden="true" />}
        </div>
      </div>

      {LIVE_DEBUG && (
        <div
          aria-hidden="true"
          className="absolute top-1 left-1 z-20 font-mono text-[10px] leading-tight text-amber-300 bg-black/80 rounded px-1.5 py-1 pointer-events-none whitespace-pre"
          data-live-debug
        >
          {[
            `rows=${frame?.rows ?? "-"} hist=${frame?.history ?? "-"} lines=${lines.length}`,
            `grid=${renderCols}cols spacer=${spacerLines} lastNonBlank=${lastNonBlankRow}`,
            `cur=${frame?.cursor ? `${frame.cursor.x},${frame.cursor.y}` : "null"} -> row=${live.row} col=${live.col}`,
            `lineH=${lineH.toFixed(2)} charW=${charW.toFixed(3)}`,
            `seq=${frame?.seq ?? "-"} alt=${altScreen ? 1 : 0} fps=${frameTiming.fps().toFixed(1)} paint=${frameTiming
              .meanPaintMs()
              .toFixed(1)}ms`,
            `transport=${transport ?? "-"} frames=${liveStats?.frames ?? "-"} patches=${
              liveStats?.patches ?? "-"
            } resyncs=${liveStats?.resyncs ?? "-"} wire=${
              liveStats ? `${(liveStats.wireBytes / 1024).toFixed(1)}k` : "-"
            }`,
          ].join("\n")}
        </div>
      )}

      {(reading || selectionHeld) && (
        <button
          type="button"
          onClick={jumpToLatest}
          aria-label="Back to live"
          className="absolute right-3 bottom-16 z-10 w-10 h-10 rounded-full bg-surface-800/90 border border-surface-700/30 text-text-secondary flex items-center justify-center shadow-lg backdrop-blur-sm active:scale-95 motion-safe:animate-[fadeIn_200ms_ease-out]"
        >
          <StrokeIcon size={16} strokeWidth="2" hidden>
            <polyline points="6 9 12 15 18 9" />
          </StrokeIcon>
        </button>
      )}

      <textarea
        ref={inputRef}
        aria-label="Live terminal input"
        className="absolute bottom-2 left-2 w-px h-px opacity-0"
        // iOS draws the caret ignoring opacity; caret-color hides it.
        style={{ fontSize: "16px", caretColor: "transparent", color: "transparent" }}
        onFocus={() => {
          setFocused(true);
          onInputFocusChange(true);
        }}
        onBlur={() => {
          setFocused(false);
          onInputFocusChange(false);
        }}
        autoCapitalize="off"
        autoCorrect="off"
        autoComplete="off"
        spellCheck={false}
        onKeyDownCapture={(e) => input.onKeyDownCapture(e.nativeEvent)}
        onKeyDown={(e) => input.onKeyDown(e.nativeEvent)}
        onPaste={(e) => input.onPaste(e.nativeEvent)}
        onCompositionStart={() => input.onCompositionStart()}
        onCompositionUpdate={(e) => input.onCompositionUpdate(e.nativeEvent)}
        onCompositionEnd={(e) => input.onCompositionEnd(e.nativeEvent)}
      />
    </div>
  );
}
