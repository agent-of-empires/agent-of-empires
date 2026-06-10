import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, RefObject } from "react";
import type { AnsiSegment, AnsiStyle } from "../lib/ansi";
import { ansiToLines } from "../lib/liveTermLines";
import type { LiveFrame } from "../hooks/useLiveTerminal";
import { BackToLiveButton } from "./BackToLiveButton";
import { useWebSettings } from "../hooks/useWebSettings";

// Mobile rendering of a tmux agent pane, mirroring the TUI's live mode:
// the server streams `capture-pane` snapshots (see src/server/live_ws.rs)
// and this component renders them as real DOM text inside a NATIVELY
// scrolling container. Scrollback is the browser's own scroll (momentum,
// 120Hz, finger-true, long-press text selection), not tmux copy-mode:
// the agent keeps running while the user reads, and there is no wheel
// synthesis, momentum re-implementation, or copy-mode state to infer.
//
// Scroll model: total virtual content = history (tmux scrollback) +
// live screen. The frame carries the pane's `history` line count, and
// `content` covers the last `window` lines of that. A spacer div stands
// in for the un-fetched history above, so the scroller's total height
// is stable: growing the capture window converts spacer rows into real
// rows 1:1 with no scroll jump, and appended output only ever grows the
// bottom edge.

const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 28;
const LINE_RATIO = 1.2;
/** How many lines each history fetch adds to the capture window. */
const WINDOW_GROW_LINES = 400;
/** Mirrors MAX_WINDOW_LINES in src/server/live_ws.rs. */
const MAX_WINDOW_LINES = 4000;
/** Resize debounce: one SIGWINCH-equivalent per settled layout. */
const RESIZE_DEBOUNCE_MS = 150;

export interface MobileLiveTerminalProps {
  frame: LiveFrame | null;
  connected: boolean;
  active: boolean;
  sendResize: (cols: number, rows: number) => void;
  setWindow: (lines: number) => void;
  setCadence: (fast: boolean) => void;
  sendData: (data: string) => void;
  /** Virtual Ctrl modifier from the mobile toolbar. */
  ctrlActiveRef: RefObject<boolean>;
  clearCtrl: () => void;
  /** Hidden input element, exposed so the keyboard FAB / toolbar can
   *  focus and blur it. */
  inputRef: RefObject<HTMLTextAreaElement | null>;
}

function segStyle(style: AnsiStyle): CSSProperties | undefined {
  const css: CSSProperties = {};
  let fg = style.fg;
  let bg = style.bg;
  if (style.inverse) {
    [fg, bg] = [bg ?? "var(--term-bg, #1c1c1f)", fg ?? "var(--term-fg, #e4e4e7)"];
  }
  if (fg) css.color = fg;
  if (bg) css.backgroundColor = bg;
  if (style.bold) css.fontWeight = 700;
  if (style.dim) css.opacity = 0.6;
  if (style.italic) css.fontStyle = "italic";
  if (style.underline) css.textDecoration = "underline";
  return Object.keys(css).length ? css : undefined;
}

const Row = memo(function Row({ segs }: { segs: AnsiSegment[] }) {
  if (segs.length === 0) {
    // Keep empty rows at full line height.
    return <div>{" "}</div>;
  }
  return (
    <div>
      {segs.map((seg, i) => (
        <span key={i} style={segStyle(seg.style)}>
          {seg.text}
        </span>
      ))}
    </div>
  );
});

/** Measure the monospace advance width for the live font at `size`. */
function measureCharWidth(size: number): number {
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d");
  if (!ctx) return size * 0.6;
  ctx.font = `${size}px 'Geist Mono', ui-monospace, 'SFMono-Regular', monospace`;
  const w = ctx.measureText("M").width;
  return w > 0 ? w : size * 0.6;
}

export function MobileLiveTerminal({
  frame,
  connected,
  active,
  sendResize,
  setWindow,
  setCadence,
  sendData,
  ctrlActiveRef,
  clearCtrl,
  inputRef,
}: MobileLiveTerminalProps) {
  const { settings, update } = useWebSettings();
  const [fontSize, setFontSize] = useState(() => settings.mobileFontSize);
  const scrollerRef = useRef<HTMLDivElement>(null);

  const lineH = fontSize * LINE_RATIO;
  const charW = useMemo(() => measureCharWidth(fontSize), [fontSize]);

  const lines = useMemo(() => (frame ? ansiToLines(frame.content) : []), [frame]);
  const screenRows = frame?.rows ?? 0;
  const history = frame?.history ?? 0;
  // Lines of history the current capture window does NOT cover; rendered
  // as a spacer so total scroll height tracks the full virtual content.
  const fetchedHistory = Math.max(0, lines.length - screenRows);
  const spacerLines = Math.max(0, history - fetchedHistory);

  // --- at-bottom tracking + cadence + window growth -------------------
  const atBottomRef = useRef(true);
  const [showBackToLive, setShowBackToLive] = useState(false);
  const windowRef = useRef(0);
  const growThrottleRef = useRef(0);
  const rowsRef = useRef(0);
  useEffect(() => {
    rowsRef.current = screenRows;
  }, [screenRows]);

  const requestWindow = useCallback(
    (lines: number) => {
      const clamped = Math.min(MAX_WINDOW_LINES, lines);
      if (clamped === windowRef.current) return;
      windowRef.current = clamped;
      setWindow(clamped);
    },
    [setWindow],
  );

  const syncCadence = useCallback(() => {
    setCadence(atBottomRef.current && active && document.visibilityState === "visible");
  }, [setCadence, active]);

  const onScroll = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
    const atBottom = distance < lineH;
    if (atBottom !== atBottomRef.current) {
      atBottomRef.current = atBottom;
      setShowBackToLive(!atBottom);
      syncCadence();
      if (atBottom) {
        // Back at the live edge: shrink the capture window so fast-
        // cadence frames stay small. The spacer reabsorbs the height.
        requestWindow(Math.max(rowsRef.current, 0) || 0);
      }
    }
    // Approaching the spacer: pull more history into the window.
    if (!atBottom && el.scrollTop < el.clientHeight * 2) {
      const now = Date.now();
      if (now - growThrottleRef.current > 300 && windowRef.current < MAX_WINDOW_LINES) {
        growThrottleRef.current = now;
        requestWindow(windowRef.current + WINDOW_GROW_LINES);
      }
    }
  }, [lineH, requestWindow, syncCadence]);

  // First upward gesture: with the window at screen size there may be
  // nothing to scroll yet, so a plain scroll event never fires. Detect a
  // downward drag at the top and seed the first history fetch.
  const touchStartYRef = useRef(0);
  const onTouchStartCapture = useCallback((e: React.TouchEvent) => {
    if (e.touches.length === 1) touchStartYRef.current = e.touches[0]!.clientY;
  }, []);
  const onTouchMoveCapture = useCallback(
    (e: React.TouchEvent) => {
      if (e.touches.length !== 1) return;
      const el = scrollerRef.current;
      if (!el) return;
      const dy = e.touches[0]!.clientY - touchStartYRef.current;
      const scrollable = el.scrollHeight > el.clientHeight + 1;
      if (dy > 24 && !scrollable && history > 0 && windowRef.current < MAX_WINDOW_LINES) {
        const now = Date.now();
        if (now - growThrottleRef.current > 300) {
          growThrottleRef.current = now;
          requestWindow(windowRef.current + WINDOW_GROW_LINES);
        }
      }
    },
    [history, requestWindow],
  );

  // --- pinch zoom (two-finger) -----------------------------------------
  const pinchRef = useRef<{ startDist: number; startSize: number; changed: boolean } | null>(null);
  const persistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onTouchStart = useCallback(
    (e: React.TouchEvent) => {
      onTouchStartCapture(e);
      if (e.touches.length === 2) {
        const [a, b] = [e.touches[0]!, e.touches[1]!];
        pinchRef.current = {
          startDist: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY),
          startSize: fontSize,
          changed: false,
        };
      }
    },
    [fontSize, onTouchStartCapture],
  );
  const onTouchMove = useCallback(
    (e: React.TouchEvent) => {
      if (e.touches.length === 2 && pinchRef.current) {
        e.preventDefault();
        const [a, b] = [e.touches[0]!, e.touches[1]!];
        const dist = Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
        const { startDist, startSize } = pinchRef.current;
        if (startDist > 0) {
          const next = Math.round(Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, startSize * (dist / startDist))));
          if (next !== startSize) pinchRef.current.changed = true;
          setFontSize(next);
        }
        return;
      }
      onTouchMoveCapture(e);
    },
    [onTouchMoveCapture],
  );
  const onTouchEnd = useCallback(
    (e: React.TouchEvent) => {
      if (e.touches.length < 2 && pinchRef.current) {
        const changed = pinchRef.current.changed;
        pinchRef.current = null;
        if (!changed) return;
        if (persistTimerRef.current) clearTimeout(persistTimerRef.current);
        persistTimerRef.current = setTimeout(() => {
          update({ mobileFontSize: fontSize });
        }, 400);
      }
    },
    [fontSize, update],
  );
  useEffect(
    () => () => {
      if (persistTimerRef.current) clearTimeout(persistTimerRef.current);
    },
    [],
  );

  // --- grid sizing ------------------------------------------------------
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el || !active) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const compute = () => {
      const cols = Math.floor(el.clientWidth / charW);
      const rows = Math.floor(el.clientHeight / lineH);
      // Implausibly small means a hidden/mid-transition container; never
      // ship that to tmux (same guard as the xterm path).
      if (cols < 20 || rows < 5) return;
      sendResize(cols, rows);
      if (windowRef.current < rows) requestWindow(rows);
    };
    const ro = new ResizeObserver(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(compute, RESIZE_DEBOUNCE_MS);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [active, charW, lineH, sendResize, requestWindow]);

  // Cadence follows tab visibility too.
  useEffect(() => {
    const onVisibility = () => syncCadence();
    document.addEventListener("visibilitychange", onVisibility);
    syncCadence();
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, [syncCadence]);

  // --- scroll anchoring on frame updates --------------------------------
  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    if (atBottomRef.current) {
      el.scrollTop = el.scrollHeight;
    }
    // Not at bottom: leave scrollTop alone. The spacer model keeps the
    // height ABOVE the viewport invariant under every transition
    // (output flow moves a line from content-top into the spacer;
    // window growth converts spacer rows into content rows 1:1), so the
    // browser-preserved scrollTop keeps the same lines in view, and new
    // output only ever extends the bottom edge.
  }, [lines, spacerLines, lineH]);

  const exitScrollback = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    atBottomRef.current = true;
    setShowBackToLive(false);
    syncCadence();
    requestWindow(rowsRef.current || 0);
  }, [requestWindow, syncCadence]);

  // --- keyboard input ----------------------------------------------------
  const composingRef = useRef(false);
  const sendKeys = useCallback(
    (data: string) => {
      if (ctrlActiveRef.current && data.length === 1) {
        const code = data.toUpperCase().charCodeAt(0);
        if (code >= 65 && code <= 90) {
          sendData(String.fromCharCode(code - 64));
          clearCtrl();
          return;
        }
      }
      sendData(data);
    },
    [sendData, ctrlActiveRef, clearCtrl],
  );

  // Native (not React-synthetic) beforeinput: React's onBeforeInput is
  // backed by keypress in Chromium and carries no inputType, so the
  // soft-keyboard input types below would never match through it.
  useEffect(() => {
    const ta = inputRef.current;
    if (!ta) return;
    const onBeforeInput = (ev: InputEvent) => {
      if (composingRef.current || ev.isComposing) return;
      switch (ev.inputType) {
        case "insertText":
          ev.preventDefault();
          if (ev.data) sendKeys(ev.data);
          break;
        case "insertLineBreak":
        case "insertParagraph":
          ev.preventDefault();
          sendKeys("\r");
          break;
        case "deleteContentBackward":
          ev.preventDefault();
          sendKeys("\x7f");
          break;
        case "insertFromPaste": {
          ev.preventDefault();
          const text = ev.data ?? "";
          if (text) {
            // Bracketed paste so agents treat embedded newlines as
            // pasted text, not per-line submits.
            sendData(`\x1b[200~${text}\x1b[201~`);
          }
          break;
        }
        default:
          break;
      }
    };
    ta.addEventListener("beforeinput", onBeforeInput);
    return () => ta.removeEventListener("beforeinput", onBeforeInput);
  }, [sendKeys, sendData, inputRef]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (composingRef.current || e.nativeEvent.isComposing) return;
      const seq = (() => {
        switch (e.key) {
          case "Enter":
            return "\r";
          case "Backspace":
            return "\x7f";
          case "Tab":
            return "\t";
          case "Escape":
            return "\x1b";
          case "ArrowUp":
            return "\x1b[A";
          case "ArrowDown":
            return "\x1b[B";
          case "ArrowRight":
            return "\x1b[C";
          case "ArrowLeft":
            return "\x1b[D";
          default:
            return null;
        }
      })();
      if (seq) {
        e.preventDefault();
        sendData(seq);
        return;
      }
      // Hardware Ctrl+letter chords (bluetooth keyboards).
      if (e.ctrlKey && !e.metaKey && !e.altKey && e.key.length === 1) {
        const code = e.key.toUpperCase().charCodeAt(0);
        if (code >= 65 && code <= 90) {
          e.preventDefault();
          sendData(String.fromCharCode(code - 64));
        }
      }
    },
    [sendData],
  );

  const onPaste = useCallback(
    (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
      e.preventDefault();
      const text = e.clipboardData.getData("text/plain");
      if (text) sendData(`\x1b[200~${text}\x1b[201~`);
    },
    [sendData],
  );

  const onCompositionStart = useCallback(() => {
    composingRef.current = true;
  }, []);
  const onCompositionEnd = useCallback(
    (e: React.CompositionEvent<HTMLTextAreaElement>) => {
      composingRef.current = false;
      if (e.data) sendKeys(e.data);
      if (inputRef.current) inputRef.current.value = "";
    },
    [sendKeys, inputRef],
  );

  // --- cursor overlay ------------------------------------------------------
  const cursor = frame?.cursor ?? null;
  const cursorTop = cursor ? (spacerLines + Math.max(0, lines.length - screenRows) + cursor.y) * lineH : 0;
  const cursorLeft = cursor ? cursor.x * charW : 0;

  return (
    <div className="absolute inset-0" data-live-terminal>
      <div
        ref={scrollerRef}
        onScroll={onScroll}
        onTouchStart={onTouchStart}
        onTouchMove={onTouchMove}
        onTouchEnd={onTouchEnd}
        onTouchCancel={onTouchEnd}
        className="absolute inset-0 overflow-y-auto overflow-x-hidden font-mono"
        style={{
          fontSize: `${fontSize}px`,
          lineHeight: `${lineH}px`,
          background: "var(--term-bg, #1c1c1f)",
          color: "var(--term-fg, #e4e4e7)",
          overscrollBehavior: "contain",
          WebkitOverflowScrolling: "touch",
        }}
      >
        <div className="relative whitespace-pre" data-live-content>
          {spacerLines > 0 && <div style={{ height: `${spacerLines * lineH}px` }} aria-hidden="true" />}
          {lines.map((segs, i) => (
            <Row key={i} segs={segs} />
          ))}
          {connected && cursor && (
            <div
              aria-hidden="true"
              className="absolute motion-safe:animate-pulse"
              data-live-cursor
              style={{
                top: `${cursorTop}px`,
                left: `${cursorLeft}px`,
                width: `${charW}px`,
                height: `${lineH}px`,
                background: "var(--term-cursor, #f59e0b)",
                opacity: 0.8,
              }}
            />
          )}
        </div>
      </div>

      {showBackToLive && <BackToLiveButton onClick={exitScrollback} topOffset="top-3" />}

      <textarea
        ref={inputRef}
        aria-label="Live terminal input"
        className="absolute bottom-2 left-2 w-px h-px opacity-0"
        style={{ fontSize: "16px" }}
        autoCapitalize="off"
        autoCorrect="off"
        autoComplete="off"
        spellCheck={false}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
        onCompositionStart={onCompositionStart}
        onCompositionEnd={onCompositionEnd}
      />
    </div>
  );
}
