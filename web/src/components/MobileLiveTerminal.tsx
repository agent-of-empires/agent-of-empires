import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, RefObject } from "react";
import type { AnsiSegment, AnsiStyle } from "../lib/ansi";
import { ansiToLines, wrapLine } from "../lib/liveTermLines";
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
  /** Focus tracking for the chrome: on touch devices focus == soft
   *  keyboard visible, the deterministic alternative to occlusion
   *  heuristics. */
  onInputFocusChange: (focused: boolean) => void;
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
  onInputFocusChange,
}: MobileLiveTerminalProps) {
  const { settings, update } = useWebSettings();
  const [fontSize, setFontSize] = useState(() => settings.mobileFontSize);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const measureRef = useRef<HTMLSpanElement>(null);

  const lineH = fontSize * LINE_RATIO;
  // Real rendered glyph advance, measured off a hidden span INSIDE the
  // scroller so it reflects whatever font is actually in effect right
  // now. A canvas measurement at mount ran before the webfont loaded on
  // a cold boot, so the cursor overlay and the cols shipped to tmux were
  // both computed from fallback metrics: the cursor sat off the cells
  // and claude drew its box at the wrong width. Re-measured when
  // `document.fonts.ready` resolves and whenever the font size changes.
  const [charW, setCharW] = useState(() => fontSize * 0.6);
  const remeasure = useCallback(() => {
    const el = measureRef.current;
    if (!el) return;
    const w = el.getBoundingClientRect().width / 20;
    if (w > 0) {
      setCharW((prev) => (Math.abs(prev - w) > 0.01 ? w : prev));
    }
  }, []);
  useLayoutEffect(() => {
    remeasure();
  }, [remeasure, fontSize]);
  useEffect(() => {
    const fonts = (document as Document & { fonts?: { ready: Promise<unknown> } }).fonts;
    fonts?.ready
      ?.then(() => remeasure())
      .catch(() => {
        // No FontFaceSet (headless/jsdom); the layout-effect measure stands.
      });
  }, [remeasure]);

  // --- frame geometry -------------------------------------------------------
  // The hook owns the read machine: `frame` is already the frozen
  // snapshot while reading, the live stream otherwise.
  const rowsRef = useRef(0);
  const readingRef = useRef(reading);
  useEffect(() => {
    readingRef.current = reading;
  }, [reading]);
  // No pinning (and no live-edge re-entry) while a finger is down: a
  // programmatic scrollTop during an active touch cancels the native
  // gesture on iOS.
  const touchActiveRef = useRef(false);
  // Geometry from BEFORE the current DOM mutation. Pinning decisions use
  // "was the user at the bottom before this content/size change", the
  // classic chat-scroll algorithm: it reads the user's position straight
  // from the DOM (scrollTop is current the instant a drag moves, ahead
  // of any scroll EVENT), so an arriving frame can never pin the
  // scroller back under a starting gesture, while appended output still
  // follows the live tail.
  //
  // The scrollTop comparison covers the gap touchActiveRef can't: a
  // flick lifts the finger immediately, and on a busy session a live
  // frame can land in the first ~50ms of momentum while the scroller
  // is still inside the at-bottom threshold. Pinning there snaps the
  // view back AND cancels iOS momentum, making scroll-up nearly
  // impossible to start. Upward motion since the last mutation means
  // the user is heading into scrollback; never pin against it.
  const geomRef = useRef({ scrollHeight: 0, clientHeight: 0, scrollTop: 0 });
  const pinIfWasAtBottom = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const prev = geomRef.current;
    const wasAtBottom = prev.scrollHeight === 0 || prev.scrollHeight - el.scrollTop - prev.clientHeight < lineH * 1.5;
    const movingUp = prev.scrollHeight !== 0 && el.scrollTop < prev.scrollTop - 0.5;
    if (wasAtBottom && !movingUp && !touchActiveRef.current) {
      el.scrollTop = el.scrollHeight;
    }
    geomRef.current = { scrollHeight: el.scrollHeight, clientHeight: el.clientHeight, scrollTop: el.scrollTop };
  }, [lineH]);
  const lines = useMemo(() => (frame ? ansiToLines(frame.content) : []), [frame]);
  // Columns this viewer renders at. Normally the pane is exactly this
  // wide and wrapping is the identity; when another writer resizes the
  // window wider (see the server-side drift re-assert), wrapping keeps
  // the frame readable instead of clipping at the right edge.
  const [renderCols, setRenderCols] = useState(0);
  const visual = useMemo(() => {
    const cols = renderCols > 0 ? renderCols : Number.POSITIVE_INFINITY;
    const rows: AnsiSegment[][] = [];
    // Visual row index where each pane line starts (for cursor math).
    const lineStartRow: number[] = new Array(lines.length);
    for (let i = 0; i < lines.length; i++) {
      lineStartRow[i] = rows.length;
      for (const row of wrapLine(lines[i]!, cols)) rows.push(row);
    }
    return { rows, lineStartRow };
  }, [lines, renderCols]);
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
    return el.scrollHeight - el.scrollTop - el.clientHeight < lineH * 1.5;
  }, [lineH]);

  const onScroll = useCallback(() => {
    if (!atBottom()) {
      enterReading(rowsRef.current);
    } else if (!touchActiveRef.current) {
      // Mid-gesture passes over the bottom edge are settled on touchend;
      // re-entering live here would let the next frame pin against the
      // user's finger.
      returnToLive(rowsRef.current);
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
      touchActiveRef.current = true;
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
      if (e.touches.length === 0) {
        touchActiveRef.current = false;
        // Settle the live-edge decision deferred by onScroll; momentum
        // scroll events after this keep re-evaluating via onScroll.
        if (atBottom()) {
          returnToLive(rowsRef.current);
        }
      }
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
    [fontSize, update, returnToLive, atBottom],
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
      setRenderCols(cols);
      sendResize(cols, rows);
      if (!readingRef.current) {
        setWindow(rows);
      }
    };
    const ro = new ResizeObserver(() => {
      // Keep the live edge pinned through layout changes (keyboard
      // open/close, toolbar mount) immediately, then settle the grid.
      pinIfWasAtBottom();
      if (timer) clearTimeout(timer);
      timer = setTimeout(compute, RESIZE_DEBOUNCE_MS);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [active, charW, lineH, sendResize, setWindow, pinIfWasAtBottom]);

  // Cadence: fast only while this pane is the active, visible surface.
  useEffect(() => {
    const sync = () => setCadence(active && document.visibilityState === "visible");
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, [active, setCadence]);

  // --- bottom pinning ---------------------------------------------------------
  useLayoutEffect(() => {
    pinIfWasAtBottom();
    // When not pinned, scrollTop is left alone. Above-viewport height is
    // invariant (spacer rows convert to content rows 1:1; appends only
    // extend the bottom), so the browser-preserved offset keeps the
    // same lines in view.
  }, [lines, spacerLines, lineH, pinIfWasAtBottom]);

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
  let cursorTop = 0;
  let cursorLeft = 0;
  if (cursor) {
    const lineIdx = Math.max(0, lines.length - screenRows) + cursor.y;
    const baseRow = visual.lineStartRow[lineIdx] ?? visual.rows.length;
    const cols = renderCols > 0 ? renderCols : Number.POSITIVE_INFINITY;
    const wrapOffset = Number.isFinite(cols) ? Math.floor(cursor.x / cols) : 0;
    cursorTop = (spacerLines + baseRow + wrapOffset) * lineH;
    cursorLeft = (Number.isFinite(cols) ? cursor.x % cols : cursor.x) * charW;
  }

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
          // The spacer model keeps above-viewport pixels invariant by
          // construction, so a preserved scrollTop is always correct.
          // The browser's own scroll anchoring doesn't know that: when
          // the full-history frame replaces the spacer it re-anchors and
          // teleports scrollTop. Ours is the only anchoring allowed.
          overflowAnchor: "none",
        }}
      >
        <span
          ref={measureRef}
          aria-hidden="true"
          className="absolute whitespace-pre"
          style={{ visibility: "hidden", pointerEvents: "none" }}
        >
          MMMMMMMMMMMMMMMMMMMM
        </span>
        <div className="relative whitespace-pre" data-live-content>
          {spacerLines > 0 && <div style={{ height: `${spacerLines * lineH}px` }} aria-hidden="true" />}
          {visual.rows.map((segs, i) => (
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
        // iOS renders the system text caret in an overlay layer that
        // IGNORES the element's opacity, so a focused hidden input grows
        // a ghost caret floating over the terminal. caret-color is the
        // documented off switch; color guards select-all artifacts.
        style={{ fontSize: "16px", caretColor: "transparent", color: "transparent" }}
        onFocus={() => onInputFocusChange(true)}
        onBlur={() => onInputFocusChange(false)}
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
