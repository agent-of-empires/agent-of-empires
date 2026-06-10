import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, RefObject } from "react";
import type { AnsiSegment, AnsiStyle } from "../lib/ansi";
import { ansiToLines } from "../lib/liveTermLines";
import type { LiveFrame } from "../hooks/useLiveTerminal";
import { useWebSettings } from "../hooks/useWebSettings";

// Mobile rendering of a tmux agent pane, mirroring the TUI's live mode:
// the server streams `capture-pane` snapshots (src/server/live_ws.rs)
// and this component renders them as real DOM text inside a NATIVELY
// scrolling container. There is no tmux copy-mode, no wheel synthesis,
// no momentum re-implementation, and the agent keeps running while the
// user reads.
//
// Reading model (mirrors the TUI's "capture window follows the scroll
// offset", adapted for a network hop):
//
//   live     — pinned to the bottom. The capture window is just the
//              screen, so frames are small and fast.
//   fetching — the user scrolled up. One window request covers the
//              ENTIRE history; the spacer (sized from tmux's
//              #{history_size}) already made the area scrollable, so a
//              flick lands wherever it lands and the content fills in
//              underneath it in one round trip.
//   held     — the full-history frame arrived. The client freezes it
//              and tells the server to stop pushing (`hold`), so the
//              reading surface cannot move and zero bytes flow while
//              reading. Returning to the bottom releases the hold; a
//              fresh frame arrives in ~one capture interval.
//
// Total scroll height is constant across all of this: spacer rows are
// converted into real rows 1:1 as content arrives, so the browser's
// preserved scrollTop keeps the same lines in view with no compensation.
//
// The soft keyboard never resizes tmux. Rows are derived from the
// LARGEST container height seen for the current width (the no-keyboard
// size); a keyboard cycle only shrinks the visible part of the
// bottom-pinned scroller, exactly like a chat app.

const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 28;
const LINE_RATIO = 1.2;
/** Resize debounce: one tmux resize per settled layout. */
const RESIZE_DEBOUNCE_MS = 150;

export interface MobileLiveTerminalProps {
  frame: LiveFrame | null;
  connected: boolean;
  active: boolean;
  /** True while the hook's read machine is off the live edge; the frame
   *  prop is then the frozen full-history snapshot. */
  reading: boolean;
  sendResize: (cols: number, rows: number) => void;
  setWindow: (lines: number) => void;
  setCadence: (fast: boolean) => void;
  enterReading: (rows: number) => void;
  returnToLive: (rows: number) => void;
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
    return <div> </div>;
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
  reading,
  sendResize,
  setWindow,
  setCadence,
  enterReading,
  returnToLive,
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

  // --- frame geometry -------------------------------------------------------
  // The hook owns the read machine: `frame` is already the frozen
  // snapshot while reading, the live stream otherwise.
  const rowsRef = useRef(0);
  const readingRef = useRef(reading);
  useEffect(() => {
    readingRef.current = reading;
  }, [reading]);
  const lines = useMemo(() => (frame ? ansiToLines(frame.content) : []), [frame]);
  const screenRows = frame?.rows ?? 0;
  const history = frame?.history ?? 0;
  const fetchedHistory = Math.max(0, lines.length - screenRows);
  const spacerLines = Math.max(0, history - fetchedHistory);
  useEffect(() => {
    rowsRef.current = screenRows || rowsRef.current;
  }, [screenRows]);

  const atBottom = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return true;
    return el.scrollHeight - el.scrollTop - el.clientHeight < lineH;
  }, [lineH]);

  const onScroll = useCallback(() => {
    if (atBottom()) {
      returnToLive(rowsRef.current);
    } else {
      enterReading(rowsRef.current);
    }
  }, [atBottom, enterReading, returnToLive]);

  const jumpToLatest = useCallback(() => {
    const el = scrollerRef.current;
    if (el) el.scrollTop = el.scrollHeight;
    returnToLive(rowsRef.current);
  }, [returnToLive]);

  // --- pinch zoom (two-finger) ---------------------------------------------
  const pinchRef = useRef<{ startDist: number; startSize: number; changed: boolean } | null>(null);
  const persistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onTouchStart = useCallback(
    (e: React.TouchEvent) => {
      if (e.touches.length === 2) {
        const [a, b] = [e.touches[0]!, e.touches[1]!];
        pinchRef.current = {
          startDist: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY),
          startSize: fontSize,
          changed: false,
        };
      }
    },
    [fontSize],
  );
  const onTouchMove = useCallback((e: React.TouchEvent) => {
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
    }
  }, []);
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

  // --- grid sizing -----------------------------------------------------------
  // Rows come from the LATCHED maximum container height for the current
  // width, so a soft-keyboard cycle (which shrinks the container) never
  // resizes tmux; the bottom-pinned scroller just shows fewer rows of an
  // unchanged screen. The latch resets when the width changes
  // (rotation, sidebar) or the font scale changes the grid anyway.
  const latchRef = useRef<{ width: number; maxHeight: number }>({ width: 0, maxHeight: 0 });
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el || !active) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const compute = () => {
      const width = el.clientWidth;
      const height = el.clientHeight;
      if (width <= 0 || height <= 0) return;
      const latch = latchRef.current;
      if (Math.abs(width - latch.width) > 1) {
        latch.width = width;
        latch.maxHeight = height;
      } else if (height > latch.maxHeight) {
        latch.maxHeight = height;
      }
      const cols = Math.floor(width / charW);
      const rows = Math.floor(latch.maxHeight / lineH);
      // Implausibly small means a hidden/mid-transition container; never
      // ship that to tmux.
      if (cols < 20 || rows < 5) return;
      rowsRef.current = rows;
      sendResize(cols, rows);
      if (!readingRef.current) {
        setWindow(rows);
      }
    };
    const ro = new ResizeObserver(() => {
      // Keep the live edge pinned through layout changes (keyboard
      // open/close, toolbar mount) immediately, then settle the grid.
      if (!readingRef.current) {
        el.scrollTop = el.scrollHeight;
      }
      if (timer) clearTimeout(timer);
      timer = setTimeout(compute, RESIZE_DEBOUNCE_MS);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [active, charW, lineH, sendResize, setWindow]);

  // Cadence: fast only while this pane is the active, visible surface.
  useEffect(() => {
    const sync = () => setCadence(active && document.visibilityState === "visible");
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, [active, setCadence]);

  // --- bottom pinning ---------------------------------------------------------
  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    if (!readingRef.current) {
      el.scrollTop = el.scrollHeight;
    }
    // fetching/held: leave scrollTop alone. Above-viewport height is
    // invariant (spacer rows convert to content rows 1:1; appends only
    // extend the bottom), so the browser-preserved offset keeps the
    // same lines in view.
  }, [lines, spacerLines, lineH]);

  // --- keyboard input -----------------------------------------------------------
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

  // --- cursor overlay (live edge only; a frozen snapshot has no cursor) -------
  const cursor = !reading ? (frame?.cursor ?? null) : null;
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

      {reading && (
        <button
          type="button"
          onClick={jumpToLatest}
          aria-label="Back to live"
          className="absolute right-3 bottom-16 z-10 w-10 h-10 rounded-full bg-surface-800/90 border border-surface-700/30 text-text-secondary flex items-center justify-center shadow-lg backdrop-blur-sm active:scale-95 motion-safe:animate-[fadeIn_200ms_ease-out]"
        >
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <polyline points="6 9 12 15 18 9" />
          </svg>
        </button>
      )}

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
