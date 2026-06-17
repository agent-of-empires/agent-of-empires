import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, ReactNode, RefObject } from "react";
import type { AnsiSegment, AnsiStyle } from "../lib/ansi";
import { ansiToLines, wrapLine } from "../lib/liveTermLines";
import { wheelNotches } from "../lib/liveMouse";
import type { LiveFrame } from "../hooks/useLiveTerminal";
import { useWebSettings } from "../hooks/useWebSettings";
import { useIsCoarsePointer } from "../hooks/useIsCoarsePointer";

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
//   live    — pinned to the live edge. The capture window is just the
//             screen, so frames are small and fast.
//   reading — the user scrolled up. One window request covers the
//             ENTIRE history; the spacer (sized from tmux's
//             #{history_size}) already made the area scrollable, so a
//             flick lands wherever it lands and the content fills in
//             underneath it in one round trip. The stream keeps flowing
//             at idle cadence (the agent runs on, like the TUI); there
//             is no hold/freeze.
//
// The reading position is stable without a freeze because above-viewport
// pixels are invariant by construction: spacer rows convert into real
// rows 1:1 as content arrives, and when the agent appends k lines the
// spacer grows by k while the capture window slides down by k, which
// cancels. The browser-preserved scrollTop keeps the same lines in view
// with no compensation.
//
// The soft keyboard never resizes tmux. Rows are derived from the
// LARGEST container height seen for the current width (the no-keyboard
// size); a keyboard cycle only shrinks the visible part of the scroller.
// While the keyboard has the container shrunk below that latched height,
// the live-edge scroll target anchors the CURSOR near the viewport
// bottom (see liveScrollTarget) so the agent's prompt stays in view; at
// full height the target is the literal bottom and the whole screen is
// visible, exactly like a terminal.

const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 28;
const LINE_RATIO = 1.2;
/** Resize debounce: one tmux resize per settled layout. */
const RESIZE_DEBOUNCE_MS = 150;
/** How long the meaningful-row scroll anchor must stay lower before it
 *  shrinks, so a spinner toggling the lowest non-blank row can't flutter the
 *  viewport. Comfortably longer than an agent's redraw cadence. */
const SHRINK_DELAY_MS = 600;

export interface MobileLiveTerminalProps {
  frame: LiveFrame | null;
  connected: boolean;
  active: boolean;
  /** True while the user reads scrollback (off the live edge); the
   *  capture window is widened and the jump-to-latest button shows.
   *  The frame keeps streaming either way. */
  reading: boolean;
  sendResize: (cols: number, rows: number) => void;
  setWindow: (lines: number) => void;
  setCadence: (fast: boolean) => void;
  enterReading: (rows: number) => void;
  returnToLive: (rows: number) => void;
  sendData: (data: string) => void;
  /** Forward a wheel notch to a full-screen mouse app (alternate screen).
   *  Used instead of capture-window scrolling when the frame reports the
   *  pane is such an app. */
  forwardWheel: (up: boolean, sgr: boolean, col: number, row: number) => void;
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
  /** Bottom-align the screen chat-style (agent surface) so a short screen's
   *  prompt sits just above the keyboard. The paired host/container shells are
   *  ordinary terminals, so they top-align like a normal bash window. */
  bottomAlign: boolean;
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

// Diagnostic overlay for cursor-alignment field reports: open the dashboard
// with `?livedebug=1` and the live view shows the geometry the overlay math
// ran on (frame rows/history, content lines, spacer, computed line index).
// Screenshot-friendly; no behavior changes.
const LIVE_DEBUG = typeof location !== "undefined" && new URLSearchParams(location.search).has("livedebug");

// Hollow-box cursor drawn AS A CELL inside the text flow, the way a real
// terminal (and the desktop xterm view) renders it, rather than a separate
// absolutely-positioned block whose pixel row we reconstruct from cursor.y and
// line-height. Tying it to the actual rendered cell means it cannot drift off
// its row (wrapping, row-height, offset assumptions). `outline` instead of
// `border` so the box does not reflow the line by a pixel.
const CURSOR_CELL_STYLE: CSSProperties = {
  outline: "1px solid var(--term-cursor, #f59e0b)",
  outlineOffset: "-1px",
};

const Row = memo(function Row({ segs, cursorCol }: { segs: AnsiSegment[]; cursorCol: number | null }) {
  if (cursorCol == null) {
    if (segs.length === 0) return <div> </div>; // keep empty rows at full height
    return (
      <div>
        {segs.map((seg, i) => (
          <span key={i} style={segStyle(seg.style)}>
            {seg.text}
          </span>
        ))}
      </div>
    );
  }
  // Render the row with the single cell at `cursorCol` boxed. Walk segments by
  // column; split the one that straddles the cursor.
  const out: ReactNode[] = [];
  let col = 0;
  let placed = false;
  let key = 0;
  for (const seg of segs) {
    const t = seg.text;
    if (!placed && cursorCol >= col && cursorCol < col + t.length) {
      const off = cursorCol - col;
      if (off > 0) {
        out.push(
          <span key={key++} style={segStyle(seg.style)}>
            {t.slice(0, off)}
          </span>,
        );
      }
      out.push(
        <span key={key++} data-live-cursor style={{ ...segStyle(seg.style), ...CURSOR_CELL_STYLE }}>
          {t[off]}
        </span>,
      );
      if (off + 1 < t.length) {
        out.push(
          <span key={key++} style={segStyle(seg.style)}>
            {t.slice(off + 1)}
          </span>,
        );
      }
      placed = true;
    } else {
      out.push(
        <span key={key++} style={segStyle(seg.style)}>
          {t}
        </span>,
      );
    }
    col += t.length;
  }
  if (!placed) {
    // Cursor sits past the row's text (blank input cell): pad to the column
    // and box a space.
    if (cursorCol > col) out.push(<span key="pad">{" ".repeat(cursorCol - col)}</span>);
    out.push(
      <span key="cursor" data-live-cursor style={CURSOR_CELL_STYLE}>
        {" "}
      </span>,
    );
  }
  return <div>{out}</div>;
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
  forwardWheel,
  ctrlActiveRef,
  clearCtrl,
  inputRef,
  onInputFocusChange,
  bottomAlign,
}: MobileLiveTerminalProps) {
  const { settings, update } = useWebSettings();
  // The live view now renders on desktop too, so it honors the right font-size
  // setting per device: the desktop terminal size on a fine pointer, the
  // (smaller) mobile size on touch. Reading the wrong one is why the desktop
  // pane came up tiny and ignored the dashboard's font-size control.
  const coarse = useIsCoarsePointer();
  const fontKey = coarse ? "mobileFontSize" : "desktopFontSize";
  const configuredFontSize = settings[fontKey];
  const [fontSize, setFontSize] = useState(() => configuredFontSize);
  // Adopt the persisted setting when it changes (settings panel, or the
  // pointer class flipping which font key applies) via the adjust-state-
  // during-render pattern. Pinch-zoom on touch still drives fontSize live
  // below; mid-gesture the setting is unchanged so this never clobbers it.
  const [lastConfiguredFontSize, setLastConfiguredFontSize] = useState(configuredFontSize);
  if (configuredFontSize !== lastConfiguredFontSize) {
    setLastConfiguredFontSize(configuredFontSize);
    setFontSize(configuredFontSize);
  }
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
  // `frame` always tracks the live stream; reading scrollback just widens
  // the capture window (the hook owns that). Nothing is frozen.
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
  //
  // The live-edge scroll target is the literal bottom, with ONE
  // exception: while the soft keyboard has the container shrunk below
  // the latched no-keyboard height, the screen is taller than the
  // viewport and a fresh agent's literal bottom is blank rows with the
  // prompt scrolled off the top. The target then anchors the CURSOR
  // near the viewport bottom instead. The cursor (parked in the agent's
  // input box) is the stable choice of anchor: pinning to the last
  // non-blank row was tried and reverted, because capture-pane catches
  // mid-repaint states whose lowest non-blank row jumps around
  // (spinner / footer redraws), and every flutter moved the viewport.
  const latchRef = useRef<{ width: number; maxHeight: number }>({ width: 0, maxHeight: 0 });
  // Pixel top of the cursor row. Sticky across frames that momentarily
  // hide the cursor (mid-redraw captures) so the target cannot flap.
  const cursorAnchorRef = useRef<number | null>(null);
  // The anchor is in pixels at the current line height; a font-scale
  // change while the cursor is hidden would leave it in the old scale,
  // so invalidate and wait for the next cursor-bearing frame.
  useEffect(() => {
    cursorAnchorRef.current = null;
  }, [lineH]);
  const liveScrollTarget = useCallback(
    (el: HTMLDivElement) => {
      const bottom = Math.max(0, el.scrollHeight - el.clientHeight);
      const shrunken = latchRef.current.maxHeight - el.clientHeight > lineH * 1.5;
      const anchor = cursorAnchorRef.current;
      if (!shrunken || anchor == null) return bottom;
      // One spare line under the cursor row keeps the input box border
      // visible beneath it.
      return Math.min(bottom, Math.max(0, anchor + 2 * lineH - el.clientHeight));
    },
    [lineH],
  );
  const geomRef = useRef({ target: -1, clientHeight: 0, scrollTop: 0 });
  // A height change observed while pinning was suppressed (finger down,
  // gesture in flight) would otherwise be consumed without effect and
  // the cursor anchor never applied; latch it until a pin actually runs.
  const pendingHeightPinRef = useRef(false);
  const pinIfWasAtBottom = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const prev = geomRef.current;
    const target = liveScrollTarget(el);
    const wasAtLive = prev.target < 0 || el.scrollTop >= prev.target - lineH * 1.5;
    const movingUp = prev.target >= 0 && el.scrollTop < prev.scrollTop - 0.5;
    // Pin downward freely (following appended output); pin upward only
    // when the viewport height changed (keyboard transition). An upward
    // pin at constant height would yank a user who deliberately
    // scrolled below the cursor anchor.
    if (prev.target >= 0 && Math.abs(el.clientHeight - prev.clientHeight) > 1) {
      pendingHeightPinRef.current = true;
    }
    if (!wasAtLive) {
      // The user is reading scrollback; a keyboard transition there
      // must not yank them later.
      pendingHeightPinRef.current = false;
    } else if (
      !movingUp &&
      !touchActiveRef.current &&
      (prev.target < 0 || pendingHeightPinRef.current || target > el.scrollTop)
    ) {
      el.scrollTop = target;
      pendingHeightPinRef.current = false;
    }
    geomRef.current = { target, clientHeight: el.clientHeight, scrollTop: el.scrollTop };
  }, [lineH, liveScrollTarget]);
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
  // Full-screen mouse app (alternate screen): its scrollback is not
  // capturable, so the spacer of unrelated normal-buffer history is
  // useless. Pin to the live edge (no spacer, no native scroll) and
  // forward the wheel to the app instead; the next frame reflects its
  // scroll. Mirrors the TUI's forward_wheel_to_live_pane.
  const forwardMode = (frame?.altScreen ?? false) && (frame?.mouse ?? false);
  const mouseSgr = frame?.mouseSgr ?? false;
  const effectiveSpacerLines = forwardMode ? 0 : spacerLines;
  const forwardModeRef = useRef(forwardMode);
  const mouseSgrRef = useRef(mouseSgr);
  useEffect(() => {
    forwardModeRef.current = forwardMode;
    mouseSgrRef.current = mouseSgr;
  }, [forwardMode, mouseSgr]);
  // Sub-notch scroll remainder (px) carried across events, and the last
  // touch Y while forwarding a single-finger drag.
  const wheelAccumRef = useRef(0);
  const touchForwardYRef = useRef<number | null>(null);
  useEffect(() => {
    rowsRef.current = screenRows || rowsRef.current;
  }, [screenRows]);

  // Last visual row with real text. A fullscreen agent (Claude) only fills
  // part of a tall mobile pane and leaves the rest blank; this is where the
  // meaningful screen ends. Cursor-independent so it drives both the
  // cursor-in-the-void check below and the no-cursor scroll anchor.
  const lastNonBlankRow = useMemo(() => {
    for (let i = visual.rows.length - 1; i >= 0; i--) {
      if (visual.rows[i]!.some((s) => s.text.trim() !== "")) return i;
    }
    return -1;
  }, [visual]);

  // Debounced count of rows to render: the last non-blank row + 1, but it
  // GROWS instantly (follow appended output) and SHRINKS only after staying
  // lower for SHRINK_DELAY_MS. Trimming the trailing blank rows lets `mt-auto`
  // bottom-align a fullscreen agent that doesn't fill the tall mobile pane, so
  // its input box sits just above the keyboard instead of floating over a dead
  // gap. The debounce is essential: a spinner toggling the lowest non-blank
  // row would otherwise change the rendered height every frame and bounce the
  // whole block (the raw last-non-blank jitter #2087 reverted). State, not a
  // ref, because the render depends on it.
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
      if (shrinkTimerRef.current == null) {
        shrinkTimerRef.current = setTimeout(() => {
          shrinkTimerRef.current = null;
          setRenderRowCount(Math.max(0, lastNonBlankRow + 1));
        }, SHRINK_DELAY_MS);
      }
      return current;
    });
  }, [lastNonBlankRow]);

  // Clear a pending shrink timer on unmount so it can't fire setRenderRowCount
  // after the component is gone. Separate from the debounce effect above so
  // its grow/shrink timing is unaffected (a deps-driven cleanup there would
  // reset the debounce on every row change).
  useEffect(
    () => () => {
      if (shrinkTimerRef.current) clearTimeout(shrinkTimerRef.current);
    },
    [],
  );

  // Cursor cell -> the VISUAL ROW + COLUMN to box inline (see Row). Shown only
  // at the live edge; reading scrollback hides it. `top` is the row's pixel
  // top, fed to cursorAnchorRef so the keyboard-shrunk scroll target can keep
  // the input row above the keyboard.
  const live = useMemo(() => {
    const cursor = !reading ? (frame?.cursor ?? null) : null;
    if (!cursor) return { row: -1, col: -1, top: null as number | null };
    const lineIdx = Math.max(0, lines.length - screenRows) + cursor.y;
    if (lineIdx < 0 || lineIdx >= lines.length) return { row: -1, col: -1, top: null };
    const cols = renderCols > 0 ? renderCols : Number.POSITIVE_INFINITY;
    const baseRow = visual.lineStartRow[lineIdx] ?? -1;
    if (baseRow < 0) return { row: -1, col: -1, top: null };
    const wrapOffset = Number.isFinite(cols) ? Math.floor(cursor.x / cols) : 0;
    const row = baseRow + wrapOffset;
    // The agent can park the hardware cursor in a trailing BLANK row below its
    // drawn UI (Claude draws its own caret in the input box higher up). Boxing
    // a cell there would put the cursor far below the input box (the reported
    // "filled rectangle 10 rows below"). When the cursor lands past the last
    // non-blank row, draw nothing; the agent's own caret stays visible.
    if (row > lastNonBlankRow) return { row: -1, col: -1, top: null };
    const col = Number.isFinite(cols) ? cursor.x % cols : cursor.x;
    return { row, col, top: (effectiveSpacerLines + row) * lineH };
  }, [reading, frame, lines.length, screenRows, visual, renderCols, effectiveSpacerLines, lineH, lastNonBlankRow]);

  const atBottom = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return true;
    // At (or below) the live-edge target counts as live: scrolling down
    // past a keyboard-shrunk cursor anchor into the screen's tail must
    // not enter reading mode.
    return el.scrollTop >= liveScrollTarget(el) - lineH * 1.5;
  }, [lineH, liveScrollTarget]);

  const onScroll = useCallback(() => {
    // Forward mode pins the live edge (overflow hidden); the wheel goes to
    // the app, so there is no scrollback reading state to enter.
    if (forwardModeRef.current) return;
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
    if (el) el.scrollTop = liveScrollTarget(el);
    returnToLive(rowsRef.current);
  }, [returnToLive, liveScrollTarget]);

  // Tap anywhere on the terminal brings up the soft keyboard, so the user does
  // not have to find the keyboard FAB. The focus() must be synchronous inside
  // the click handler for iOS to honor the user-gesture requirement for showing
  // the keyboard, so nothing async runs before it. The active-element check
  // skips a redundant re-focus when the keyboard is already up, and a click that
  // ends a text selection is left alone so select-to-copy still works (this view
  // renders on desktop too). The FAB and "Back to live" button are siblings of
  // the scroller, not descendants, so tapping them never reaches this handler.
  const focusInputOnTap = useCallback(() => {
    if (document.activeElement === inputRef.current) return;
    const sel = window.getSelection();
    if (sel && !sel.isCollapsed) return;
    inputRef.current?.focus();
  }, [inputRef]);

  // Map a viewport point to the app's 1-based pane cell for the forwarded
  // wheel event (apps mostly ignore the exact cell, but send a sane one).
  const pointerCell = useCallback(
    (clientX: number, clientY: number) => {
      const el = scrollerRef.current;
      if (!el || charW <= 0 || lineH <= 0) return { col: 1, row: 1 };
      const r = el.getBoundingClientRect();
      const cols = renderCols > 0 ? renderCols : 1;
      const rows = Math.max(1, screenRows || rowsRef.current);
      const col = Math.min(cols, Math.max(1, Math.floor((clientX - r.left) / charW) + 1));
      const row = Math.min(rows, Math.max(1, Math.floor((clientY - r.top) / lineH) + 1));
      return { col, row };
    },
    [charW, lineH, renderCols, screenRows],
  );

  // Translate an accumulated pixel delta (positive = toward newer/down)
  // into forwarded wheel notches, one per text row, carrying the leftover.
  const forwardWheelDelta = useCallback(
    (deltaPx: number, clientX: number, clientY: number) => {
      wheelAccumRef.current += deltaPx;
      const { notches, remainder } = wheelNotches(wheelAccumRef.current, lineH || 16, 8);
      wheelAccumRef.current = remainder;
      if (notches === 0) return;
      const { col, row } = pointerCell(clientX, clientY);
      const up = notches < 0;
      for (let i = 0; i < Math.abs(notches); i++) forwardWheel(up, mouseSgrRef.current, col, row);
    },
    [lineH, pointerCell, forwardWheel],
  );

  const onWheel = useCallback(
    (e: React.WheelEvent) => {
      if (!forwardModeRef.current) return;
      // Normalize line/page deltas to pixels so a notch is ~one row.
      const factor = e.deltaMode === 1 ? lineH || 16 : e.deltaMode === 2 ? (lineH || 16) * (rowsRef.current || 1) : 1;
      forwardWheelDelta(e.deltaY * factor, e.clientX, e.clientY);
    },
    [lineH, forwardWheelDelta],
  );

  // --- pinch zoom (two-finger) ---------------------------------------------
  const pinchRef = useRef<{ startDist: number; startSize: number; changed: boolean } | null>(null);
  const persistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onTouchStart = useCallback(
    (e: React.TouchEvent) => {
      touchActiveRef.current = true;
      if (e.touches.length === 2) {
        pinchRef.current = {
          startDist: Math.hypot(
            e.touches[0]!.clientX - e.touches[1]!.clientX,
            e.touches[0]!.clientY - e.touches[1]!.clientY,
          ),
          startSize: fontSize,
          changed: false,
        };
        touchForwardYRef.current = null;
      } else if (e.touches.length === 1 && forwardModeRef.current) {
        // Single-finger drag drives the app's wheel in forward mode.
        touchForwardYRef.current = e.touches[0]!.clientY;
        wheelAccumRef.current = 0;
      }
    },
    [fontSize],
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
      if (e.touches.length === 1 && forwardModeRef.current && touchForwardYRef.current != null) {
        // Stop the (overflow-hidden) container / page from scrolling and
        // translate the drag into wheel notches. Finger moving DOWN reveals
        // older content = wheel up, so the delta is negated.
        e.preventDefault();
        const y = e.touches[0]!.clientY;
        const dy = y - touchForwardYRef.current;
        touchForwardYRef.current = y;
        forwardWheelDelta(-dy, e.touches[0]!.clientX, y);
      }
    },
    [forwardWheelDelta],
  );
  const onTouchEnd = useCallback(
    (e: React.TouchEvent) => {
      if (e.touches.length === 0) {
        touchActiveRef.current = false;
        touchForwardYRef.current = null;
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
          update({ [fontKey]: fontSize });
        }, 400);
      }
    },
    [fontKey, fontSize, update, returnToLive, atBottom],
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
  // resizes tmux; the scroller just shows fewer rows of an unchanged
  // screen, anchored at the cursor (see liveScrollTarget). The latch
  // resets when the width changes (rotation, sidebar) or the font scale
  // changes the grid anyway. Resizing tmux on every keyboard cycle was
  // tried and reverted: on the capture+network path it flashed the pane
  // (blank-then-redraw) and clipped scrollback.
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

  // Cadence: fast only while this pane is the active, visible surface AND
  // at the live edge. Reading scrollback drops to idle: the window is
  // wide (big frames), and the reader is not watching the live tail.
  useEffect(() => {
    const sync = () => setCadence(active && document.visibilityState === "visible" && !reading);
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, [active, reading, setCadence]);

  // --- bottom pinning ---------------------------------------------------------
  useLayoutEffect(() => {
    // Refresh the cursor anchor before pinning so this commit pins
    // against the current frame's cursor. Sticky on purpose: a
    // mid-redraw capture that momentarily hides the cursor keeps the
    // last known anchor instead of flapping the target to the literal
    // bottom and back.
    if (live.top != null) cursorAnchorRef.current = live.top;
    pinIfWasAtBottom();
    // When not pinned, scrollTop is left alone. Above-viewport height is
    // invariant (spacer rows convert to content rows 1:1; appends only
    // extend the bottom), so the browser-preserved offset keeps the
    // same lines in view.
  }, [lines, spacerLines, lineH, live, pinIfWasAtBottom]);

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

  // The cursor is rendered inline by Row (see below): this is the visual row
  // to box, and the column within it. -1 means draw nothing.
  const cursorRow = connected && !reading ? live.row : -1;

  // Trim trailing blank rows (for bottom-align) ONLY at the live edge. While
  // reading scrollback the spacer model keeps above-viewport pixels invariant
  // so the position holds as the agent streams; trimming there would change
  // scrollHeight under the reader and snap the viewport.
  const visibleRowCount = reading ? visual.rows.length : renderRowCount;

  return (
    <div className="absolute inset-0" data-live-terminal>
      <div
        ref={scrollerRef}
        onScroll={onScroll}
        onWheel={onWheel}
        onClick={focusInputOnTap}
        onTouchStart={onTouchStart}
        onTouchMove={onTouchMove}
        onTouchEnd={onTouchEnd}
        onTouchCancel={onTouchEnd}
        className={`absolute inset-0 font-mono flex flex-col ${
          forwardMode ? "overflow-hidden" : "overflow-y-auto overflow-x-hidden"
        }`}
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
        {/* `mt-auto` bottom-aligns the screen when the rendered rows are
            shorter than the viewport (a fullscreen agent only fills part of a
            tall mobile pane), so its input box sits just above the keyboard
            instead of floating over a dead gap. When content overflows
            (scrollback) the auto margin collapses and it scrolls normally,
            sidestepping the flex+overflow top-clip bug. The paired shells
            opt out (`bottomAlign=false`) so a near-empty bash prompt sits at
            the top like a normal terminal. */}
        <div className={`relative whitespace-pre ${bottomAlign ? "mt-auto" : ""}`} data-live-content>
          {effectiveSpacerLines > 0 && (
            <div style={{ height: `${effectiveSpacerLines * lineH}px` }} aria-hidden="true" />
          )}
          {visual.rows.slice(0, visibleRowCount).map((segs, i) => (
            <Row key={i} segs={segs} cursorCol={i === cursorRow ? live.col : null} />
          ))}
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
          ].join("\n")}
        </div>
      )}

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
