import { useCallback, useEffect, useRef, useSyncExternalStore } from "react";
import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { WebglAddon } from "@xterm/addon-webgl";
import type {
  ActivateMessage,
  PauseOutputMessage,
  PrimaryStatusMessage,
  ResizeMessage,
  ResumeOutputMessage,
} from "../lib/types";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { reportTelemetrySeen } from "../lib/api";
import { getToken } from "../lib/token";
import { useWebSettings } from "./useWebSettings";
import { TerminalTiming } from "../lib/terminalTiming";
import type { TimingPingMessage, TimingPongMessage } from "../lib/types";

// Client-side terminal WS debug logging is gated behind a runtime flag
// so production users don't get a console full of lifecycle chatter.
// Two opt-ins, both checked once and cached: `localStorage.aoeDebug = '1'`
// (sticky across reloads) or `?debug=1` URL param (per-tab). Mirrors the
// server's AOE_LOG_LEVEL/AOE_TERMINAL_TRACE pair so a full triage trace
// can be captured by setting both.
const TERMINAL_DEBUG_ENABLED = (() => {
  if (typeof window === "undefined") return false;
  try {
    if (window.localStorage?.getItem("aoeDebug") === "1") return true;
  } catch {
    // localStorage can throw (Safari private mode, sandboxed iframes).
    // Fall through to URL param.
  }
  try {
    const params = new URLSearchParams(window.location.search);
    if (params.get("debug") === "1") return true;
  } catch {
    // location.search can throw in pathological embeds; treat as off.
  }
  return false;
})();
const tdbg = (...args: unknown[]) => {
  if (!TERMINAL_DEBUG_ENABLED) return;
  console.debug("[terminal.ws]", ...args);
};
const twarn = (...args: unknown[]) => {
  // Warnings are always emitted (cheap, low-volume, useful in the wild
  // even without the debug toggle). Use console.warn so DevTools filters
  // can surface terminal-specific issues quickly.
  console.warn("[terminal.ws]", ...args);
};

// Keystroke-to-echo latency instrumentation, gated on its own
// `?debug=terminal-timing` flag (separate from the `?debug=1` logging
// gate above). When off, none of the timing code runs and no probe
// traffic is sent, so normal sessions pay nothing. See #1453.
const TERMINAL_TIMING_ENABLED = (() => {
  if (typeof window === "undefined") return false;
  try {
    const params = new URLSearchParams(window.location.search);
    return params.get("debug") === "terminal-timing";
  } catch {
    return false;
  }
})();
// Cadence of the WS control-path ping while timing is enabled. 500ms
// gives ~120 samples/min, enough for p90 over a debug session without
// looking abusive on metered links.
const TIMING_PING_INTERVAL_MS = 500;
// How often the rolling p50/p95 summary is logged to the console.
const TIMING_SUMMARY_INTERVAL_MS = 10000;

// Fast-start retry schedule: 200ms, 400ms, 800ms, 1.5s, 3s, 6s, 10s. Total
// to exhaustion ~22s vs the old exponential ladder's ~91s. The old 1s/30s
// curve magnified first-session-open pain when tmux warm-up briefly bounced
// the upgrade: by the time the server was ready, the client was asleep on
// a 30s timer. See #1455.
const RETRY_DELAYS_MS = [200, 400, 800, 1500, 3000, 6000, 10000] as const;
const MAX_RETRIES = RETRY_DELAYS_MS.length;
/** Server-side close code that signals "PTY relay permanently broken,
 *  stop retrying immediately." Mirrors `CLOSE_CODE_PTY_DEAD` in
 *  `src/server/ws.rs`. Picked from the application-reserved 4000-4999
 *  range. See #1107. */
const CLOSE_CODE_PTY_DEAD = 4001;
export const retryDelayMs = (attempt: number) => {
  const idx = Math.max(1, Math.min(RETRY_DELAYS_MS.length, attempt)) - 1;
  return RETRY_DELAYS_MS[idx]!;
};
const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 28;
const DEFAULT_FONT_SIZE = 14;
const MOBILE_BREAKPOINT_PX = 768;
const WHEEL_ZOOM_SENSITIVITY = 0.05;
const WHEEL_PERSIST_DEBOUNCE_MS = 400;
// How long, after a drag-select releases, to wait for tmux's OSC 52
// clipboard escape to arrive over the WS before giving up. The escape is
// emitted by tmux's copy-selection on mouse release and round-trips through
// the PTY relay, so it lands a frame or two after `mouseup`. See #1499.
const OSC52_CLIPBOARD_TIMEOUT_MS = 500;
// Pointer travel (px) below which a press/release is treated as a click, not
// a drag, so a plain focus click doesn't arm a clipboard write.
const DRAG_COPY_THRESHOLD_PX = 4;
const RESIZE_DEBOUNCE_MS = 50;
// First-resize debounce: longer than the steady-state value so the
// initial layout transition (sidebar mount, splitter snap, font swap)
// settles into a single PTY resize instead of one per stable point.
// CSS transitions in the dashboard run ~200ms; 250ms covers them with
// a small margin. After the first resize lands the debounce drops to
// RESIZE_DEBOUNCE_MS so live splitter drags still feel responsive.
const INITIAL_SETTLE_MS = 250;

const FONT_FAMILY = "'Geist Mono', ui-monospace, 'SFMono-Regular', monospace";

export interface TerminalState {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  isPrimary: boolean;
  /**
   * True when the user has scrolled up and tmux is (likely) in copy-mode.
   * Set when the first wheel-up byte goes out after being false; cleared
   * by an explicit call to `exitScrollback()` from the "Back to live" UI.
   * We use the client-side send as the signal rather than a server-sent
   * notification because tmux copy-mode state is not exposed on the PTY.
   */
  isInScrollback: boolean;
}

/**
 * Read the 16 ANSI + bg/fg/cursor slots out of CSS custom properties on
 * documentElement (set by useResolvedTheme) and return an xterm.js ITheme.
 * Called at terminal construction and again on `aoe:theme-changed` so a
 * live palette swap doesn't require a reconnect.
 */
export function readThemeFromCss(): ITheme {
  const root = document.documentElement;
  const cs = getComputedStyle(root);
  const v = (name: string, fallback: string) => cs.getPropertyValue(name).trim() || fallback;
  return {
    background: v("--term-bg", "#1c1c1f"),
    foreground: v("--term-fg", "#e4e4e7"),
    cursor: v("--term-cursor", "#f59e0b"),
    cursorAccent: v("--term-bg", "#1c1c1f"),
    selectionBackground: "rgba(161, 161, 170, 0.35)",
    black: v("--term-color-0", "#1c1c1f"),
    red: v("--term-color-1", "#ef4444"),
    green: v("--term-color-2", "#22c55e"),
    yellow: v("--term-color-3", "#fbbf24"),
    blue: v("--term-color-4", "#0d9488"),
    magenta: v("--term-color-5", "#f59e0b"),
    cyan: v("--term-color-6", "#0d9488"),
    white: v("--term-color-7", "#e4e4e7"),
    brightBlack: v("--term-color-8", "#8b8b94"),
    brightRed: v("--term-color-9", "#f26969"),
    brightGreen: v("--term-color-10", "#4ed17e"),
    brightYellow: v("--term-color-11", "#fccc50"),
    brightBlue: v("--term-color-12", "#3da9a0"),
    brightMagenta: v("--term-color-13", "#f7b13c"),
    brightCyan: v("--term-color-14", "#3da9a0"),
    brightWhite: v("--term-color-15", "#fbbf24"),
  };
}

/**
 * Manages an xterm.js terminal connected to a PTY-relayed WebSocket.
 * Returns a ref to attach to a container div, plus connection state.
 *
 * `claudeFullscreen` is read at connect time (the connect effect's deps
 * are intentionally only `[sessionId, wsPath]`). Toggling Claude's
 * `/tui` setting mid-session won't take effect on the live terminal;
 * the user has to reattach. That matches Claude Code itself, which also
 * needs a restart to switch renderers.
 */
export function useTerminal(
  sessionId: string | null,
  wsPath: string = "ws",
  autoFocus: boolean = true,
  claudeFullscreen: boolean = false,
  claimPrimary: boolean = true,
) {
  const { settings, update } = useWebSettings();
  const claimPrimaryRef = useRef(claimPrimary);
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const countdownRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const retryCountRef = useRef(0);
  // Shared ref so the onData callback can read the virtual Ctrl state
  // set by MobileTerminalToolbar. This bridges React state with the
  // native event handler without requiring focus on the proxy input.
  const ctrlActiveRef = useRef(false);
  // Stable callback set by the component to clear React's ctrlActive state
  // when onData consumes the Ctrl modifier.
  const clearCtrlRef = useRef<(() => void) | null>(null);
  // Populated inside the effect; `exitScrollback()` uses it to reset the
  // mobile scroll-depth counter when the user escapes copy-mode.
  const resetScrollbackDepthRef = useRef<(() => void) | null>(null);
  // Populated inside the effect; `exitScrollback()` uses it to cancel any
  // in-flight momentum decay so post-flick wheel-ups don't immediately
  // re-enter scrollback after the user taps "Back to live".
  const cancelMomentumRef = useRef<(() => void) | null>(null);
  // Mirror of state.isInScrollback so the resize callback can read the
  // latest value without re-creating the terminal. Updated by an effect
  // below.
  const isInScrollbackRef = useRef(false);
  // Latest pending resize that was deferred because the user was reading
  // scrollback. Drained when scrollback exits.
  const pendingResizeRef = useRef<{ cols: number; rows: number } | null>(null);
  // Most recent size measured by FitAddon. Until this is populated the
  // ws.onopen path skips its initial resize so we don't push xterm's
  // default 80x24 to the server before the container has been measured.
  const lastMeasuredRef = useRef<{ cols: number; rows: number } | null>(null);
  // Set inside the effect; the scrollback-watch effect calls it to flush
  // a deferred resize without poking React state.
  const flushPendingResizeRef = useRef<(() => void) | null>(null);
  // Set inside the effect to point at the local `connect()` function so
  // `manualReconnect` (defined outside the effect closure) can dial a
  // fresh WS directly when the prior socket is already CLOSED. Calling
  // ws.close() on a CLOSED socket is a no-op, which was the bug behind
  // the dead Retry button after retries exhausted. See #1009.
  const connectRef = useRef<(() => void) | null>(null);
  // Fire the `web_terminal` usage signal once per hook lifetime, not on every
  // reconnect: ws.onopen runs again after a WiFi blip, and the telemetry intent
  // is "this terminal was opened", not "the socket reconnected N times".
  const telemetrySeenRef = useRef(false);
  // Reverse pointer so the `online` / `pageshow` listeners installed
  // inside the connect-effect can call manualReconnect (defined below
  // the effect) without re-running the effect itself.
  const manualReconnectRef = useRef<(() => void) | null>(null);
  const terminalStoreRef = useRef<{
    snapshot: TerminalState;
    listeners: Set<() => void>;
  } | null>(null);
  if (terminalStoreRef.current == null) {
    terminalStoreRef.current = {
      snapshot: {
        connected: false,
        reconnecting: false,
        retryCount: 0,
        retryCountdown: 0,
        isPrimary: true,
        isInScrollback: false,
      },
      listeners: new Set(),
    };
  }
  const terminalSetState = useCallback((fn: (prev: TerminalState) => TerminalState) => {
    const store = terminalStoreRef.current!;
    store.snapshot = fn(store.snapshot);
    store.listeners.forEach((l) => l());
  }, []);
  const terminalSubscribe = useCallback((listener: () => void) => {
    terminalStoreRef.current!.listeners.add(listener);
    return () => {
      terminalStoreRef.current!.listeners.delete(listener);
    };
  }, []);
  const terminalGetSnapshot = useCallback(() => terminalStoreRef.current!.snapshot, []);
  const state = useSyncExternalStore(terminalSubscribe, terminalGetSnapshot);

  const settingsRef = useRef(settings);
  const updateRef = useRef(update);
  const autoFocusRef = useRef(autoFocus);
  const claudeFullscreenRef = useRef(claudeFullscreen);

  useEffect(() => {
    claimPrimaryRef.current = claimPrimary;
    settingsRef.current = settings;
    updateRef.current = update;
    autoFocusRef.current = autoFocus;
    claudeFullscreenRef.current = claudeFullscreen;
  }, [claimPrimary, settings, update, autoFocus, claudeFullscreen]);

  useEffect(() => {
    if (!sessionId || !containerRef.current) return;

    // Clean up previous instance
    wsRef.current?.close();
    termRef.current?.dispose();
    if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
    if (countdownRef.current) clearInterval(countdownRef.current);
    retryCountRef.current = 0;

    const container = containerRef.current;
    container.innerHTML = "";

    // Latency instrumentation lives for the lifetime of this terminal and
    // is exposed on `window.__aoeTiming` so an operator can call
    // `window.__aoeTiming.dump()` to pull raw samples for offline
    // analysis. Null (and entirely inert) unless the flag is set.
    const timing = TERMINAL_TIMING_ENABLED ? new TerminalTiming() : null;
    let timingPingTimer: ReturnType<typeof setInterval> | null = null;
    let timingSummaryTimer: ReturnType<typeof setInterval> | null = null;
    if (timing) {
      (window as Window & { __aoeTiming?: TerminalTiming }).__aoeTiming = timing;
    }

    const isMobileViewport = () => window.innerWidth < MOBILE_BREAKPOINT_PX;
    const readFontSize = () =>
      isMobileViewport() ? settingsRef.current.mobileFontSize : settingsRef.current.desktopFontSize;
    const persistFontSize = (size: number) => {
      if (isMobileViewport()) updateRef.current({ mobileFontSize: size });
      else updateRef.current({ desktopFontSize: size });
    };
    const fontSize = readFontSize();

    // Child element so the container div keeps its own layout (absolute
    // inset-0 in TerminalView, flex-1 in RightPanel). xterm.js renders
    // its grid inside this element and adds the `.xterm` class.
    const termEl = document.createElement("div");
    termEl.style.width = "100%";
    termEl.style.height = "100%";
    container.appendChild(termEl);

    const term = new Terminal({
      fontFamily: FONT_FAMILY,
      fontSize,
      lineHeight: 1.2,
      theme: readThemeFromCss(),
      cursorBlink: true,
      // tmux owns scrollback. Zero here so xterm.js doesn't keep a
      // parallel scrollback above the live area (which would
      // double-count with tmux and break the "wheel-up enters tmux
      // copy-mode" model the rest of this hook relies on).
      scrollback: 0,
      allowProposedApi: true,
      convertEol: false,
    });
    termRef.current = term;
    const fitAddon = new FitAddon();
    fitRef.current = fitAddon;
    term.loadAddon(fitAddon);
    term.loadAddon(new WebLinksAddon());

    term.open(termEl);

    // Select-to-copy bridge. tmux owns text selection here: mouse-mode is on
    // (src/tmux/utils.rs) so a drag drives tmux copy-mode, which is the only
    // way to select across the scrollback boundary given xterm's
    // `scrollback: 0`. On copy, tmux (`set-clipboard on`) emits an OSC 52
    // clipboard escape, but xterm.js has no built-in OSC 52 handler, so the
    // payload was dropped on the floor and select-to-copy silently failed
    // after the wterm -> xterm.js swap. We decode it and write it to the
    // system clipboard. The escape arrives asynchronously over the WS, after
    // the `mouseup` that triggered the copy, so a bare
    // `navigator.clipboard.writeText()` is rejected for lacking a user
    // gesture; `armClipboardCopy()` below pre-arms a gesture-bound
    // `ClipboardItem` promise that this handler resolves. See #1499.
    let osc52Resolve: ((text: string) => void) | null = null;
    // Monotonic arm counter: each drag-arm captures its own seq so a
    // stale timeout from a prior arm cannot null out a newer arm's
    // resolver (which would make the next OSC 52 miss the gesture-bound
    // path). See #1499.
    let osc52ArmSeq = 0;
    const writeClipboardText = (text: string) => {
      navigator.clipboard?.writeText(text).catch((err) => {
        // Expected on browsers that won't honor an async write without a
        // live user gesture (notably Firefox, which lacks promise-valued
        // ClipboardItem). Best-effort; logged, not surfaced.
        tdbg("clipboard writeText failed", err);
      });
    };
    term.parser.registerOscHandler(52, (data) => {
      // Payload shape: "<targets>;<base64>" where targets is e.g. "c"
      // (clipboard), "p" (primary), or "cp". A lone "?" in the data half is
      // a paste query we don't answer.
      const sep = data.indexOf(";");
      if (sep === -1) return true;
      const encoded = data.slice(sep + 1);
      if (encoded === "" || encoded === "?") return true;
      let text: string;
      try {
        const bin = atob(encoded);
        const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
        text = new TextDecoder().decode(bytes);
      } catch (err) {
        tdbg("osc52 decode failed", err);
        return true;
      }
      if (osc52Resolve) {
        osc52Resolve(text);
        osc52Resolve = null;
      } else {
        // Not a user drag (e.g. the agent itself ran a copy); write directly.
        writeClipboardText(text);
      }
      return true;
    });

    // Mobile soft-keyboard Backspace autorepeat arrives as a stream of
    // `beforeinput` events (inputType "deleteContentBackward", with keydown
    // surfacing as "Unidentified" / keyCode 229). xterm decodes only the
    // first into a single onData, so holding Backspace deletes one character
    // instead of repeat-deleting; the PTY sees one DEL rather than one per
    // tick. Intercept on xterm's hidden textarea and emit one DEL per tick
    // ourselves. preventDefault blocks the textarea mutation so xterm's
    // input-diff decoder never fires its own onData for the same tick (no
    // double-delete). Gated to coarse-pointer-without-fine-pointer: a
    // hardware Backspace fires a recognized keydown that xterm preventDefaults,
    // which per the UI Events spec suppresses the follow-on beforeinput, so
    // this path only runs for soft keyboards. Mirrors the mobile-Enter
    // interception in Composer.tsx. See #1450.
    const xtermTextarea = termEl.querySelector("textarea");
    const onBeforeInput = (e: InputEvent) => {
      if (e.inputType !== "deleteContentBackward" || e.isComposing) return;
      const coarse = window.matchMedia?.("(pointer: coarse)").matches;
      const anyFine = window.matchMedia?.("(any-pointer: fine)").matches;
      if (!coarse || anyFine) return;
      const ws = wsRef.current;
      if (ws?.readyState !== WebSocket.OPEN) return;
      e.preventDefault();
      ws.send(new TextEncoder().encode("\x7f"));
    };
    xtermTextarea?.addEventListener("beforeinput", onBeforeInput, {
      capture: true,
    });

    // Shift+Enter → bracketed paste containing a newline. Agents like
    // Claude Code treat pasted newlines as literal text (inserted, not
    // submitted). Bracketed paste passes cleanly through tmux without
    // requiring extended-keys negotiation. Gated on bare Shift+Enter so
    // Ctrl/Alt/Cmd+Shift+Enter still reach the app as distinct combos.
    term.attachCustomKeyEventHandler((ev) => {
      if (ev.key === "Enter" && ev.shiftKey && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
        if (ev.type === "keydown") {
          const ws = wsRef.current;
          if (ws?.readyState === WebSocket.OPEN) {
            ws.send(new TextEncoder().encode("\x1b[200~\n\x1b[201~"));
          }
        }
        return false;
      }
      return true;
    });

    // GPU renderer. Loaded after .open() per the addon's contract. Falls
    // back to the DOM renderer silently on machines where the context is
    // unavailable (Safari private mode, headless CI, software-render VMs)
    // so the terminal still works there.
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        timing?.setRenderer("dom");
        webgl.dispose();
      });
      term.loadAddon(webgl);
      timing?.setRenderer("webgl");
    } catch (err) {
      timing?.setRenderer("dom");
      tdbg("webgl addon unavailable, using DOM renderer", err);
    }

    // Resize messaging. FitAddon measures the container and calls
    // term.resize(cols, rows), which triggers term.onResize below.
    // Debounce the WS message so a splitter drag or keyboard
    // animation collapses into a single SIGWINCH at the resting size.
    let resizeDebounceTimer: ReturnType<typeof setTimeout> | null = null;
    // Flips true once the first measured resize has been sent. Until
    // then, onResize uses INITIAL_SETTLE_MS so the dashboard's mount-
    // time layout transitions coalesce into one PTY resize instead of
    // one per stable point along the way.
    let hasSentInitialResize = false;

    // All client-initiated resize sends route through this helper so the
    // scrollback gate is impossible to bypass. While the user is reading
    // scrollback, hold the latest size and drain it on exit. Without the
    // gate, claude redraws on every SIGWINCH and stacks banners into
    // tmux scrollback while the user is trying to read it.
    //
    // Also dedupes consecutive identical sizes. The ws.onopen path and
    // the rAF re-send both read from lastMeasuredRef, so back-to-back
    // calls with the same cols/rows are common; sending both would
    // produce two SIGWINCHes for one effective resize.
    let lastSentCols = -1;
    let lastSentRows = -1;
    const sendResize = (cols: number, rows: number) => {
      if (isInScrollbackRef.current) {
        pendingResizeRef.current = { cols, rows };
        return;
      }
      // Skip sends whose dimensions came from measuring a hidden
      // container. ContentSplit mounts the paired terminal twice on
      // desktop (the inline copy and the mobile slide-in overlay, the
      // latter hidden via Tailwind `md:hidden`). xterm.js inside the
      // hidden copy lays out at a tiny grid and tries to ship its
      // ~10x4 measurement to the same tmux session that the visible
      // copy is attached to. tmux honors the smallest attached
      // client's size, so the visible terminal ends up rendering its
      // shell into a 10x4 pane bordered by DEC line-drawing chars.
      //
      // We treat offsetParent==null + implausibly small dimensions as
      // the hidden-container signal. The dual condition keeps the
      // Vitest jsdom suite green: jsdom returns null offsetParent for
      // everything regardless of layout, but the mock terminal there
      // proposes a real-shaped grid that comfortably clears the
      // threshold.
      if (!termEl.offsetParent && (cols < 20 || rows < 5)) return;
      if (cols === lastSentCols && rows === lastSentRows) return;
      const ws = wsRef.current;
      if (ws?.readyState !== WebSocket.OPEN) return;
      ws.send(JSON.stringify({ type: "resize", cols, rows } as ResizeMessage));
      lastSentCols = cols;
      lastSentRows = rows;
    };

    term.onResize(({ cols, rows }) => {
      lastMeasuredRef.current = { cols, rows };
      if (resizeDebounceTimer) clearTimeout(resizeDebounceTimer);
      const delay = hasSentInitialResize ? RESIZE_DEBOUNCE_MS : INITIAL_SETTLE_MS;
      resizeDebounceTimer = setTimeout(() => {
        resizeDebounceTimer = null;
        hasSentInitialResize = true;
        sendResize(cols, rows);
      }, delay);
    });

    // Initial fit, scheduled AFTER onResize is registered so the first
    // term.resize() call from the fit emits to our callback. xterm.js's
    // open() triggers layout, so the container has its real size by the
    // time we get here; fit() populates lastMeasuredRef before ws.onopen
    // fires so the 80x24 default never reaches the server. RAF backup
    // covers the case where the container hasn't laid out yet (panel
    // mounts mid-transition).
    try {
      fitAddon.fit();
    } catch {
      // ignore -- the RAF + ResizeObserver below will retry
    }
    const initialFitRaf = requestAnimationFrame(() => {
      try {
        fitAddon.fit();
      } catch {
        // fit() throws if the container has zero rows/cols; harmless
        // here because the ResizeObserver below will retry.
      }
    });

    // Refit on container resize. xterm.js has no built-in autoResize so
    // we wire ResizeObserver directly. Skip zero-sized observations,
    // which fire while the element is being attached/detached.
    //
    // We also call `term.resize(proposed.cols, proposed.rows)` directly
    // instead of relying on FitAddon.fit() to do it: fit() reads xterm's
    // cached cell metrics, and on the side-panel mount path the first
    // sync fit ran against a still-laying-out container, latched a tiny
    // grid (e.g. 10x4), and subsequent fits at the correct container
    // size would propose the same 10x4 because xterm's internal cell
    // metrics had been re-derived from the wrong grid. Computing the
    // proposed dimensions and pushing them through term.resize each
    // observation breaks that latch and reliably propagates the final
    // container size up to the server.
    let lastObservedWidth = -1;
    let lastObservedHeight = -1;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const w = entry.contentRect.width;
        const h = entry.contentRect.height;
        if (w <= 0 || h <= 0) continue;
        if (Math.abs(w - lastObservedWidth) < 1 && Math.abs(h - lastObservedHeight) < 1) {
          continue;
        }
        lastObservedWidth = w;
        lastObservedHeight = h;
        try {
          const proposed = fitAddon.proposeDimensions();
          if (
            proposed &&
            proposed.cols > 0 &&
            proposed.rows > 0 &&
            (proposed.cols !== term.cols || proposed.rows !== term.rows)
          ) {
            term.resize(proposed.cols, proposed.rows);
          }
        } catch {
          // ignore transient measurement failures
        }
      }
    });
    ro.observe(termEl);

    // Re-fit once webfonts have fully loaded. The synchronous initial
    // fit may have used fallback-font metrics on a cold cache; once
    // Geist Mono loads, the cell width changes and the grid needs to
    // be recomputed.
    const fontsApi = (document as Document & { fonts?: { ready: Promise<unknown> } }).fonts;
    if (fontsApi?.ready) {
      fontsApi.ready
        .then(() => {
          if (termRef.current !== term) return;
          try {
            fitAddon.fit();
          } catch {
            // ignore
          }
        })
        .catch(() => {
          // fonts.ready can reject in headless environments where no
          // FontFaceSet is wired up; treat as no-op.
        });
    }

    // Drain handler: sends the latest deferred size when the user
    // exits scrollback. Routes through sendResize, but by this point
    // isInScrollbackRef is false so it takes the live-send path.
    flushPendingResizeRef.current = () => {
      const pending = pendingResizeRef.current;
      pendingResizeRef.current = null;
      if (!pending) return;
      sendResize(pending.cols, pending.rows);
    };

    connectRef.current = connect;
    function connect() {
      openSocket();
    }
    function openSocket() {
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      // Pass the auth token via the WebSocket subprotocol list instead of
      // the URL query string. URLs land in access logs (axum, cloudflared,
      // Tailscale, any reverse proxy); subprotocol headers don't.
      const token = getToken();
      const url = `${proto}//${location.host}/sessions/${sessionId}/${wsPath}`;
      tdbg("connect()", {
        sessionId,
        wsPath,
        url,
        tokenPresent: !!token,
        attempt: retryCountRef.current,
      });
      // Carry the device-binding secret as a subprotocol so the
      // middleware can authenticate the WS upgrade (passphrase
      // second factor) in addition to the token. See #1131.
      let bindingSecret: string | null = null;
      try {
        bindingSecret = getOrCreateDeviceBindingSecret();
      } catch {
        // Storage/crypto unavailable; let the server reject so the
        // login page surfaces the failure rather than booting into a
        // broken terminal.
      }
      const protocols: string[] = ["aoe-auth"];
      if (token) protocols.push(token);
      if (bindingSecret) protocols.push(`aoe-device.${bindingSecret}`);
      const ws = new WebSocket(url, protocols);
      ws.binaryType = "arraybuffer";
      wsRef.current = ws;

      // Per-connection flag: flipped on the first message received so
      // the retry counter only resets when the relay is demonstrably
      // alive end-to-end. WS handshake completion (`onopen`) is not
      // proof: a permanently broken pane accepts the upgrade and
      // closes within milliseconds, and resetting on `onopen` made
      // the retry counter loop at (1/MAX) forever. See #1107.
      let hasReceivedData = false;
      ws.onopen = () => {
        tdbg("ws.onopen", {
          sessionId,
          readyState: ws.readyState,
          protocol: ws.protocol,
        });
        if (!telemetrySeenRef.current) {
          telemetrySeenRef.current = true;
          reportTelemetrySeen("web_terminal");
        }
        // Reset the dedup baseline so the first resize on a fresh
        // connection always reaches the server, even if it matches
        // the size we last sent on the previous (now-closed) socket.
        // The new server-side handler may not share state with the
        // old one (think tunnel restarts) and needs to learn the
        // current PTY size from scratch.
        lastSentCols = -1;
        lastSentRows = -1;
        // Preserve isInScrollback across reconnects. Tmux's copy-mode
        // state is stored on the pane and survives client disconnects,
        // so the client-side flag should too — otherwise a WiFi blip
        // mid-scroll would hide the "Back to live" button while tmux
        // is still in copy-mode, leaving the user with no way out.
        terminalSetState((prev) => ({
          ...prev,
          connected: true,
          reconnecting: false,
          isPrimary: true,
        }));
        if (autoFocusRef.current) term.focus();
        // Claim primary immediately for visible terminals so this
        // client's resize is applied. Hidden persistent terminals keep
        // their socket warm without stealing primary from the active tab.
        if (claimPrimaryRef.current) {
          ws.send(JSON.stringify({ type: "activate" } as ActivateMessage));
        }
        // Send initial PTY dimensions only if FitAddon has actually
        // measured the container. Reading term.cols/term.rows directly
        // would yield xterm's 80x24 default before fit() runs, and
        // pushing that ahead of the real measurement causes a
        // stale-default -> real-size resize storm at session open. The
        // onResize callback (already wired through sendResize) delivers
        // the correct size after the first measurement, so on the very
        // first connect this branch is intentionally a no-op. On
        // reconnect lastMeasuredRef is populated and we send
        // immediately so the new server-side handler picks up the
        // right size.
        const measured = lastMeasuredRef.current;
        if (measured) {
          sendResize(measured.cols, measured.rows);
        }
        // Re-send after layout settles. Same gate; on first connect
        // this still no-ops because the ResizeObserver fires async.
        requestAnimationFrame(() => {
          const m = lastMeasuredRef.current;
          if (m) {
            sendResize(m.cols, m.rows);
          }
        });
      };

      ws.onmessage = (event: MessageEvent) => {
        if (!hasReceivedData) {
          // First payload byte: relay is alive end-to-end. Reset the
          // retry counter here (not in `onopen`) so a server that
          // accepts the upgrade then immediately closes can't keep
          // the counter pinned at 1 forever. See #1107.
          hasReceivedData = true;
          retryCountRef.current = 0;
          terminalSetState((prev) => ({
            ...prev,
            retryCount: 0,
            retryCountdown: 0,
          }));
        }
        if (event.data instanceof ArrayBuffer) {
          const bytes = new Uint8Array(event.data);
          const token = timing?.onBinaryFrame(performance.now());
          if (token) {
            // This frame resolved an armed keystroke; capture the time
            // through xterm's render completion as well as socket arrival.
            term.write(bytes, () => timing!.onRender(token, performance.now()));
          } else {
            term.write(bytes);
          }
        } else if (typeof event.data === "string") {
          // Check for server control messages before writing to terminal
          try {
            const msg = JSON.parse(event.data) as { type?: string };
            if (msg.type === "timing_pong") {
              const pong = msg as TimingPongMessage;
              timing?.onPong(pong.seq, pong.client_t, pong.server_busy_us, performance.now());
              return;
            }
            if (msg.type === "primary_status") {
              const status = msg as PrimaryStatusMessage;
              terminalSetState((prev) => ({
                ...prev,
                isPrimary: status.is_primary,
              }));
              return;
            }
          } catch {
            // Not JSON, treat as terminal text
          }
          term.write(event.data);
        }
      };

      ws.onclose = (event: CloseEvent) => {
        const closeInfo = {
          sessionId,
          code: event.code,
          reason: event.reason,
          wasClean: event.wasClean,
          attempt: retryCountRef.current,
        };
        terminalSetState((prev) => ({ ...prev, connected: false }));
        // Server-signalled "stop retrying" (close code 4001): the PTY
        // relay is permanently broken (pane killed, tmux session
        // destroyed, etc.) and another reconnect would just immediately
        // close again. Jump straight to the retries-exhausted state so
        // the user sees the manual reconnect banner instead of a
        // silent retry loop. See #1107.
        if (event.code === CLOSE_CODE_PTY_DEAD) {
          retryCountRef.current = MAX_RETRIES;
        }
        if (retryCountRef.current < MAX_RETRIES) {
          retryCountRef.current += 1;
          const count = retryCountRef.current;
          const delayMs = retryDelayMs(count);
          let countdown = Math.ceil(delayMs / 1000);

          tdbg("ws.onclose -> scheduling retry", {
            ...closeInfo,
            nextAttempt: count,
            delayMs,
          });

          terminalSetState((prev) => ({
            ...prev,
            connected: false,
            reconnecting: true,
            retryCount: count,
            retryCountdown: countdown,
          }));

          term.write(
            `\r\n\x1b[33m[Disconnected (code=${event.code}${event.reason ? ` ${event.reason}` : ""}), reconnecting in ${countdown}s... (${count}/${MAX_RETRIES})]\x1b[0m\r\n`,
          );

          countdownRef.current = setInterval(() => {
            countdown -= 1;
            if (countdown > 0) {
              terminalSetState((prev) => ({
                ...prev,
                retryCountdown: countdown,
              }));
            }
          }, 1000);

          retryTimerRef.current = setTimeout(() => {
            if (countdownRef.current) clearInterval(countdownRef.current);
            tdbg("retry timer fired, calling connect()", { attempt: count });
            connect();
          }, delayMs);
        } else {
          twarn("ws.onclose -> retries exhausted", closeInfo);
          term.write(
            `\r\n\x1b[31m[Connection lost (code=${event.code}${event.reason ? ` ${event.reason}` : ""}). Click retry or press Enter to reconnect.]\x1b[0m\r\n`,
          );
          terminalSetState((prev) => ({
            ...prev,
            connected: false,
            reconnecting: false,
            retryCount: retryCountRef.current,
            retryCountdown: 0,
          }));
        }
      };

      ws.onerror = (event: Event) => {
        // onclose will fire after onerror; log here so debug.log captures
        // both sides of the failure (the close path only sees code/reason,
        // not the underlying transport error type).
        twarn("ws.onerror", {
          sessionId,
          readyState: ws.readyState,
          type: event.type,
        });
      };

      // Relay keystrokes as binary. When the virtual Ctrl button is armed,
      // intercept single printable characters and transform them to their
      // Ctrl equivalents (Ctrl+A = 0x01, Ctrl+U = 0x15, etc.).
      term.onData((data: string) => {
        if (ws.readyState !== WebSocket.OPEN) return;
        // Arm an Idle-TTFB sample for this keystroke. Arming (not the
        // exact byte) is all the tracker needs; it only records when the
        // terminal was idle and resolves on the next inbound frame.
        timing?.onKeystroke(performance.now());
        if (ctrlActiveRef.current && data.length === 1) {
          const code = data.toUpperCase().charCodeAt(0);
          if (code >= 65 && code <= 90) {
            ws.send(new TextEncoder().encode(String.fromCharCode(code - 64)));
            ctrlActiveRef.current = false;
            clearCtrlRef.current?.();
            return;
          }
        }
        ws.send(new TextEncoder().encode(data));
      });
    }

    // Kick off the connection. xterm.js's open() is synchronous so we
    // can dial the WS immediately after construction.
    connect();

    // Drive the control-path ping and the periodic console summary. Both
    // exist only when timing is enabled. The ping measures pure WS round
    // trip (never reaches the PTY); the summary logs rolling percentiles
    // so an operator watching the console sees attribution without
    // dumping. makePing skips while a keystroke sample is armed so the
    // probe never contends with the experiential measurement.
    if (timing) {
      timingPingTimer = setInterval(() => {
        const ws = wsRef.current;
        timing.pruneTimeouts(performance.now());
        if (ws?.readyState !== WebSocket.OPEN) return;
        const ping = timing.makePing(performance.now());
        if (ping) {
          ws.send(
            JSON.stringify({
              type: "timing_ping",
              seq: ping.seq,
              client_t: ping.client_t,
            } as TimingPingMessage),
          );
        }
      }, TIMING_PING_INTERVAL_MS);
      timingSummaryTimer = setInterval(() => {
        timing.pruneTimeouts(performance.now());
        console.info(timing.summaryLine());
      }, TIMING_SUMMARY_INTERVAL_MS);
    }

    // Touch swipe emits SGR mouse-wheel escape sequences to the PTY
    // so tmux mouse-mode enters copy-mode and scrolls.
    //
    // Track net wheel-UP depth so the client knows whether tmux is in
    // copy-mode and can pause/resume the pane's process accordingly.
    // Tmux doesn't signal copy-mode state over the PTY, so the client
    // infers it from scroll direction: depth goes 0 → 1 on first
    // wheel-UP (copy-mode entered), back to 0 when balanced (copy-mode
    // auto-exited via tmux's `-e` flag on desktop, or manually exited
    // via the "Back to live" button on mobile).
    //
    // Mobile-only: clamp wheel-DOWN emissions so depth floors at 1,
    // preventing tmux's `-e` auto-exit. On mobile the down-swipe
    // overshoots easily and the snap-to-live discards the scroll
    // position. Desktop keeps the unclamped behavior — scroll-down-past-
    // bottom auto-exits, as users expect there.
    //
    // Pause/resume apply to BOTH platforms: claude's continued output
    // shifts scrollback under the reader regardless of client size.
    const wheelSeq = (dir: "up" | "down", cell: { col: number; row: number }) =>
      `\x1b[<${dir === "up" ? 64 : 65};${cell.col};${cell.row}M`;
    const wheelGrid = () => {
      const sentGrid = lastSentCols > 0 && lastSentRows > 0 ? { cols: lastSentCols, rows: lastSentRows } : null;
      // While reading tmux scrollback, resize messages are deferred to
      // avoid SIGWINCH redraws. xterm may still refit to a new monitor
      // size, so mouse coordinates must stay in the pane size tmux
      // actually knows about.
      if (isInScrollbackRef.current && sentGrid) return sentGrid;
      return {
        cols: Math.max(1, term.cols),
        rows: Math.max(1, term.rows),
      };
    };
    const cellFromClientPoint = (clientX: number, clientY: number, eventTarget?: EventTarget | null) => {
      const targetEl = eventTarget instanceof HTMLElement ? eventTarget : null;
      const targetRect = targetEl?.getBoundingClientRect();
      const el = targetRect && targetRect.width > 0 && targetRect.height > 0 ? targetEl : term.element;
      if (!el) return { col: 1, row: 1 };
      const rect = el.getBoundingClientRect();
      const grid = wheelGrid();
      const cellWidth = rect.width > 0 ? rect.width / grid.cols : 0;
      const cellHeight = rect.height > 0 ? rect.height / grid.rows : 0;
      const rawCol = cellWidth > 0 ? Math.floor((clientX - rect.left) / cellWidth) + 1 : 1;
      const rawRow = cellHeight > 0 ? Math.floor((clientY - rect.top) / cellHeight) + 1 : 1;
      return {
        col: Math.max(1, Math.min(grid.cols, rawCol)),
        row: Math.max(1, Math.min(grid.rows, rawRow)),
      };
    };
    const centerCell = () => {
      const grid = wheelGrid();
      return {
        col: Math.max(1, Math.ceil(grid.cols / 2)),
        row: Math.max(1, Math.ceil(grid.rows / 2)),
      };
    };
    let scrollbackDepth = 0;
    const sendWheel = (dir: "up" | "down", count: number, cell = centerCell()) => {
      const ws = wsRef.current;
      if (ws?.readyState !== WebSocket.OPEN) return;

      // Fullscreen renderer path: Claude Code manages its own virtualized
      // scrollback inside the alt screen, so tmux copy-mode is never
      // engaged. Skip the depth tracking and the pause/resume dance.
      // Just emit raw wheel sequences and let Claude's renderer handle
      // them. isInScrollback stays false; downstream UI (BackToLiveButton)
      // hides itself accordingly.
      if (claudeFullscreenRef.current) {
        const seq = wheelSeq(dir, cell);
        for (let i = 0; i < count; i++) {
          ws.send(new TextEncoder().encode(seq));
        }
        return;
      }

      let sendCount = count;
      const clampForMobile = isMobileViewport();
      if (dir === "up") {
        scrollbackDepth += sendCount;
      } else if (clampForMobile) {
        const maxDown = Math.max(0, scrollbackDepth - 1);
        sendCount = Math.min(sendCount, maxDown);
        if (sendCount === 0) return;
        scrollbackDepth -= sendCount;
      } else {
        // Desktop: emit freely, let tmux's -e handle exit. Track depth
        // so the resume transition fires when the user scrolls back.
        scrollbackDepth = Math.max(0, scrollbackDepth - sendCount);
      }
      const seq = wheelSeq(dir, cell);
      for (let i = 0; i < sendCount; i++) {
        ws.send(new TextEncoder().encode(seq));
      }
      // Transition into scrollback on first wheel-up (desktop + mobile).
      if (dir === "up") {
        terminalSetState((prev) => {
          if (prev.isInScrollback) return prev;
          if (ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: "pause_output" } as PauseOutputMessage));
          }
          return { ...prev, isInScrollback: true };
        });
      } else if (scrollbackDepth === 0) {
        // Back at live on desktop (tmux auto-exited copy-mode via -e);
        // resume the pane's process. On mobile this branch never fires
        // because the clamp keeps depth >= 1; mobile exits via the
        // explicit "Back to live" button (see exitScrollback).
        terminalSetState((prev) => {
          if (!prev.isInScrollback) return prev;
          if (ws.readyState === WebSocket.OPEN) {
            ws.send(
              JSON.stringify({
                type: "resume_output",
              } as ResumeOutputMessage),
            );
          }
          return { ...prev, isInScrollback: false };
        });
      }
    };
    // Expose so exitScrollback can reset the depth in sync with the
    // Escape sent to tmux.
    const resetScrollbackDepth = () => {
      scrollbackDepth = 0;
    };
    resetScrollbackDepthRef.current = resetScrollbackDepth;

    let touchMidY = 0;
    let touchAccum = 0;
    let lastMoveTs = 0;
    let velocity = 0;
    let momentumRaf: number | null = null;
    let gestureMode: "single-scroll" | "pinch" | "scroll" | null = null;
    let pinchStartDist = 0;
    let pinchStartSize = DEFAULT_FONT_SIZE;
    let pinchStartMidY = 0;
    let touchMidX = 0;
    let singleStartY = 0;
    let singleX = 0;
    let singleY = 0;
    let singleAccum = 0;
    let singleLastTs = 0;
    let suppressNextClick = false;
    const GESTURE_LOCK_PX = 12;
    const LINES_PER_WHEEL = 2;
    const MAX_VELOCITY = 2.0;
    const MAX_WHEELS_PER_FRAME = 6;
    const clampV = (v: number) => Math.max(-MAX_VELOCITY, Math.min(MAX_VELOCITY, v));
    const currentFontSize = (): number =>
      typeof term.options.fontSize === "number" ? term.options.fontSize : DEFAULT_FONT_SIZE;
    const pxPerWheel = () => currentFontSize() * 1.2 * LINES_PER_WHEEL;
    const prefersReducedMotion = () => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;

    const midpointY = (e: TouchEvent) => {
      const a = e.touches[0];
      const b = e.touches[1];
      if (!a || !b) return 0;
      return (a.clientY + b.clientY) / 2;
    };

    const touchDistance = (e: TouchEvent) => {
      const a = e.touches[0];
      const b = e.touches[1];
      if (!a || !b) return 0;
      return Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
    };

    const clampFont = (n: number) => Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, n));

    // Font size updates. Coalesce to one per frame, refit after change
    // so the cell grid recomputes against the new metrics.
    let pendingFontSize: number | null = null;
    let fontSizeRaf: number | null = null;
    const applyFontSize = (size: number) => {
      const next = clampFont(Math.round(size));
      const current = currentFontSize();
      if (next !== current) {
        term.options.fontSize = next;
        try {
          fitAddon.fit();
        } catch {
          // ignore
        }
      }
      return next;
    };
    const scheduleFontSize = (size: number) => {
      pendingFontSize = clampFont(Math.round(size));
      if (fontSizeRaf !== null) return;
      fontSizeRaf = requestAnimationFrame(() => {
        fontSizeRaf = null;
        if (pendingFontSize !== null) {
          applyFontSize(pendingFontSize);
          pendingFontSize = null;
        }
      });
    };
    const flushFontSize = () => {
      if (fontSizeRaf !== null) {
        cancelAnimationFrame(fontSizeRaf);
        fontSizeRaf = null;
      }
      if (pendingFontSize !== null) {
        applyFontSize(pendingFontSize);
        pendingFontSize = null;
      }
    };
    const currentPendingOrLiveSize = () => pendingFontSize ?? currentFontSize();

    const cancelMomentum = () => {
      if (momentumRaf !== null) {
        cancelAnimationFrame(momentumRaf);
        momentumRaf = null;
      }
    };
    cancelMomentumRef.current = cancelMomentum;

    const onTouchStart = (e: TouchEvent) => {
      cancelMomentum();
      suppressNextClick = false;

      if (e.touches.length === 1) {
        const t = e.touches[0]!;
        singleStartY = t.clientY;
        singleX = t.clientX;
        singleY = t.clientY;
        singleAccum = 0;
        singleLastTs = performance.now();
        velocity = 0;
        gestureMode = null;
        return;
      }

      if (e.touches.length === 2) {
        gestureMode = null;
        touchMidX = (e.touches[0]!.clientX + e.touches[1]!.clientX) / 2;
        touchMidY = midpointY(e);
        touchAccum = 0;
        velocity = 0;
        lastMoveTs = performance.now();
        pinchStartDist = touchDistance(e);
        pinchStartSize = currentFontSize();
        pinchStartMidY = touchMidY;
      }
    };

    const onTouchMove = (e: TouchEvent) => {
      // Single-finger scroll
      if (e.touches.length === 1 && (gestureMode === null || gestureMode === "single-scroll")) {
        const t = e.touches[0]!;
        const y = t.clientY;
        const now = performance.now();
        singleX = t.clientX;

        if (gestureMode === null) {
          if (Math.abs(y - singleStartY) < GESTURE_LOCK_PX) {
            singleLastTs = now;
            return;
          }
          gestureMode = "single-scroll";
          singleY = y;
        }

        e.preventDefault();

        const dy = singleY - y;
        singleY = y;
        singleAccum += dy;
        const step = pxPerWheel();
        const rawWheels = Math.trunc(singleAccum / step);
        const wheels = Math.max(-MAX_WHEELS_PER_FRAME, Math.min(MAX_WHEELS_PER_FRAME, rawWheels));
        if (wheels !== 0) {
          sendWheel(wheels > 0 ? "up" : "down", Math.abs(wheels), cellFromClientPoint(singleX, y, e.currentTarget));
          singleAccum -= wheels * step;
          const dt = Math.max(1, now - singleLastTs);
          velocity = clampV(dy / dt);
        }
        singleLastTs = now;
        return;
      }

      // Two-finger gesture (scroll or pinch)
      if (e.touches.length !== 2) return;
      e.preventDefault();
      touchMidX = (e.touches[0]!.clientX + e.touches[1]!.clientX) / 2;
      const y = midpointY(e);
      const now = performance.now();
      const dist = touchDistance(e);

      if (gestureMode === null || gestureMode === "single-scroll") {
        const distDelta = Math.abs(dist - pinchStartDist);
        const panDelta = Math.abs(y - pinchStartMidY);
        if (Math.max(distDelta, panDelta) < GESTURE_LOCK_PX) {
          lastMoveTs = now;
          return;
        }
        gestureMode = distDelta > panDelta ? "pinch" : "scroll";
        touchMidY = y;
      }

      if (gestureMode === "pinch") {
        if (pinchStartDist > 0) {
          scheduleFontSize(pinchStartSize * (dist / pinchStartDist));
        }
        lastMoveTs = now;
        return;
      }

      const dy = touchMidY - y;
      touchMidY = y;
      touchAccum += dy;
      const step = pxPerWheel();
      const rawWheels = Math.trunc(touchAccum / step);
      const wheels = Math.max(-MAX_WHEELS_PER_FRAME, Math.min(MAX_WHEELS_PER_FRAME, rawWheels));
      if (wheels !== 0) {
        sendWheel(wheels > 0 ? "up" : "down", Math.abs(wheels), cellFromClientPoint(touchMidX, y, e.currentTarget));
        touchAccum -= wheels * step;
        const dt = Math.max(1, now - lastMoveTs);
        velocity = clampV(dy / dt);
      }
      lastMoveTs = now;
    };

    const onTouchEnd = (e: TouchEvent) => {
      if (e.touches.length > 0) return;
      if (gestureMode === "pinch") {
        flushFontSize();
        persistFontSize(currentFontSize());
        gestureMode = null;
        velocity = 0;
        return;
      }
      const wasScrolling = gestureMode === "single-scroll" || gestureMode === "scroll";
      gestureMode = null;
      if (wasScrolling) suppressNextClick = true;
      if (prefersReducedMotion() || Math.abs(velocity) < 0.05) {
        velocity = 0;
        return;
      }
      let v = velocity;
      let last = performance.now();
      let carry = 0;
      const decay = () => {
        const now = performance.now();
        const dt = now - last;
        last = now;
        v *= Math.pow(0.92, dt / 16);
        carry += v * dt;
        const step = pxPerWheel();
        const rawW = Math.trunc(carry / step);
        const w = Math.max(-MAX_WHEELS_PER_FRAME, Math.min(MAX_WHEELS_PER_FRAME, rawW));
        if (w !== 0) {
          sendWheel(w > 0 ? "up" : "down", Math.abs(w), centerCell());
          carry -= w * step;
        }
        if (Math.abs(v) > 0.05) {
          momentumRaf = requestAnimationFrame(decay);
        } else {
          momentumRaf = null;
        }
      };
      momentumRaf = requestAnimationFrame(decay);
    };

    // Attach touch handlers to the .xterm element. `touch-action: none`
    // tells the browser we own all touch behavior here, so iOS Safari
    // won't engage native scroll/rubber-band on the dead-zone frames
    // before our handler decides whether to preventDefault.
    const viewport = term.element!;
    viewport.style.touchAction = "none";
    const touchOpts = { passive: false, capture: true } as const;
    viewport.addEventListener("touchstart", onTouchStart, touchOpts);
    viewport.addEventListener("touchmove", onTouchMove, touchOpts);
    viewport.addEventListener("touchend", onTouchEnd, touchOpts);
    viewport.addEventListener("touchcancel", onTouchEnd, touchOpts);

    // On mobile, suppress ALL click-to-focus so the keyboard is only
    // controlled via the FAB button. On desktop, only suppress after a
    // scroll gesture.
    const onClickCapture = (e: MouseEvent) => {
      const wasScroll = suppressNextClick;
      suppressNextClick = false;
      if (isMobileViewport() || wasScroll) e.stopPropagation();
    };
    viewport.addEventListener("click", onClickCapture, true);

    // Arm a clipboard write tied to the synchronous `mouseup` that ends a
    // drag-select, so the OSC 52 escape tmux emits asynchronously over the WS
    // keeps the browser's user-gesture authorization (see the OSC 52 handler
    // registered after term.open). A promise-valued ClipboardItem is the
    // gesture-preserving path (Safari/Chromium); browsers without it
    // (Firefox) fall back to a best-effort writeText, which may be blocked.
    // See #1499.
    const armClipboardCopy = () => {
      const armSeq = ++osc52ArmSeq;
      let settled = false;
      const finish = () => {
        settled = true;
        // Only retract the shared resolver if it still belongs to this
        // arm; a later drag may have already installed its own.
        if (osc52ArmSeq === armSeq) osc52Resolve = null;
      };
      try {
        if (typeof ClipboardItem !== "undefined" && navigator.clipboard?.write) {
          const pending = new Promise<Blob>((resolve, reject) => {
            osc52Resolve = (text) => {
              if (settled || osc52ArmSeq !== armSeq) return;
              finish();
              resolve(new Blob([text], { type: "text/plain" }));
            };
            setTimeout(() => {
              if (settled || osc52ArmSeq !== armSeq) return;
              finish();
              reject(new Error("osc52 clipboard timeout"));
            }, OSC52_CLIPBOARD_TIMEOUT_MS);
          });
          // Attach our own rejection handler so a timeout never surfaces as an
          // unhandled rejection if the clipboard implementation drops the
          // promise (the write() consumer below also sees it; both fire).
          pending.catch(() => {});
          navigator.clipboard.write([new ClipboardItem({ "text/plain": pending })]).catch(() => {
            // Rejected when no OSC 52 arrived within the timeout (drag
            // selected nothing) or the engine declined the async write.
            // Harmless.
          });
          return;
        }
      } catch {
        // Promise-valued ClipboardItem unsupported (Firefox); fall through to
        // the best-effort writeText path below.
      }
      osc52Resolve = (text) => {
        if (settled || osc52ArmSeq !== armSeq) return;
        finish();
        writeClipboardText(text);
      };
      setTimeout(() => {
        if (!settled && osc52ArmSeq === armSeq) finish();
      }, OSC52_CLIPBOARD_TIMEOUT_MS);
    };
    let mouseDownPoint: { x: number; y: number } | null = null;
    const onMouseDownCapture = (e: MouseEvent) => {
      if (e.button === 0) mouseDownPoint = { x: e.clientX, y: e.clientY };
    };
    const onWindowMouseUp = (e: MouseEvent) => {
      const start = mouseDownPoint;
      mouseDownPoint = null;
      if (e.button !== 0 || !start) return;
      // Threshold filters plain focus clicks; only a real drag (which tmux
      // turns into a copy-mode selection) should arm a clipboard write.
      if (Math.hypot(e.clientX - start.x, e.clientY - start.y) < DRAG_COPY_THRESHOLD_PX) {
        return;
      }
      armClipboardCopy();
    };
    // mousedown on the viewport so only drags that begin inside the terminal
    // count; mouseup on the window so a release outside the terminal bounds
    // (the user dragged past the top edge into scrollback) still arms.
    viewport.addEventListener("mousedown", onMouseDownCapture, {
      capture: true,
    });
    window.addEventListener("mouseup", onWindowMouseUp, { capture: true });

    // Mouse wheel: Ctrl+wheel = zoom (trackpad pinch), plain wheel =
    // scroll. tmux manages its own scrollback via mouse-mode escape
    // sequences, so we always synthesize SGR wheel sequences and emit
    // them ourselves rather than letting xterm.js auto-forward; the
    // depth tracking + mobile clamp + pause/resume need to wrap each
    // emission. Returning false from the custom handler tells xterm.js
    // to skip its own wheel processing.
    let wheelAccum = 0;
    let scrollWheelAccum = 0;
    let wheelPersistTimer: ReturnType<typeof setTimeout> | null = null;
    let lastCapturedWheelCell: { col: number; row: number } | null = null;
    const onWheelCapture = (e: WheelEvent) => {
      lastCapturedWheelCell = cellFromClientPoint(e.clientX, e.clientY, e.currentTarget ?? e.target);
    };
    viewport.addEventListener("wheel", onWheelCapture, {
      capture: true,
      passive: true,
    });
    term.attachCustomWheelEventHandler((e: WheelEvent) => {
      e.preventDefault();

      if (e.ctrlKey) {
        // Trackpad pinch fires wheel events with ctrlKey=true
        wheelAccum -= e.deltaY * WHEEL_ZOOM_SENSITIVITY;
        if (Math.abs(wheelAccum) < 1) return false;
        const delta = Math.trunc(wheelAccum);
        wheelAccum -= delta;
        const base = currentPendingOrLiveSize();
        const next = clampFont(Math.round(base + delta));
        if (next === base) return false;
        scheduleFontSize(next);
        if (wheelPersistTimer) clearTimeout(wheelPersistTimer);
        wheelPersistTimer = setTimeout(() => {
          flushFontSize();
          persistFontSize(currentFontSize());
          wheelPersistTimer = null;
        }, WHEEL_PERSIST_DEBOUNCE_MS);
        return false;
      }

      // Plain scroll: convert to SGR mouse-wheel sequences for tmux
      scrollWheelAccum += e.deltaY;
      const step = pxPerWheel();
      const rawWheels = Math.trunc(scrollWheelAccum / step);
      const wheels = Math.max(-MAX_WHEELS_PER_FRAME, Math.min(MAX_WHEELS_PER_FRAME, rawWheels));
      if (wheels !== 0) {
        const eventCell = cellFromClientPoint(e.clientX, e.clientY, e.currentTarget ?? e.target);
        const cell =
          eventCell.col === 1 && eventCell.row === 1 && lastCapturedWheelCell ? lastCapturedWheelCell : eventCell;
        sendWheel(wheels > 0 ? "down" : "up", Math.abs(wheels), cell);
        scrollWheelAccum -= wheels * step;
      }
      return false;
    });

    // When the user switches to this tab/window, tell the server so it
    // can claim primary and resize the PTY to match this viewport.
    const sendActivate = () => {
      if (!claimPrimaryRef.current) return;
      const ws = wsRef.current;
      if (ws?.readyState === WebSocket.OPEN) {
        const msg: ActivateMessage = { type: "activate" };
        ws.send(JSON.stringify(msg));
      }
    };
    // Auto-reconnect probe for "viewport just came back" events. iOS
    // Safari (and Chrome's bfcache restore) can suspend a tab in a way
    // that drops the WS onclose; the socket is CLOSED on resume but
    // the retry-driver never fired, so the user sees a frozen terminal
    // until they hit Retry. Triggering manualReconnect on these events
    // wakes the WS without user input. Bail when the socket is OPEN
    // (or still actively CONNECTING) so we don't disrupt a live
    // session. See #1009.
    const tryAutoReconnect = (label: string) => {
      const ws = wsRef.current;
      const readyState = ws?.readyState;
      if (readyState === WebSocket.OPEN || readyState === WebSocket.CONNECTING) {
        return;
      }
      tdbg("auto-reconnect", { trigger: label, readyState });
      manualReconnectRef.current?.();
    };
    const onVisibilityChange = () => {
      tdbg("visibilitychange", {
        state: document.visibilityState,
        readyState: wsRef.current?.readyState,
      });
      if (document.visibilityState === "visible") {
        sendActivate();
        tryAutoReconnect("visibilitychange");
      }
    };
    const onWindowFocus = () => {
      tdbg("window.focus", { readyState: wsRef.current?.readyState });
      sendActivate();
    };
    const onOnline = () => tryAutoReconnect("online");
    const onPageShow = (e: PageTransitionEvent) => {
      tdbg("pageshow", {
        persisted: e.persisted,
        readyState: wsRef.current?.readyState,
      });
      tryAutoReconnect("pageshow");
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    window.addEventListener("focus", onWindowFocus);
    window.addEventListener("online", onOnline);
    window.addEventListener("pageshow", onPageShow);

    return () => {
      cancelMomentum();
      cancelAnimationFrame(initialFitRaf);
      ro.disconnect();
      document.removeEventListener("visibilitychange", onVisibilityChange);
      window.removeEventListener("focus", onWindowFocus);
      window.removeEventListener("online", onOnline);
      window.removeEventListener("pageshow", onPageShow);
      viewport.removeEventListener("touchstart", onTouchStart, touchOpts);
      viewport.removeEventListener("touchmove", onTouchMove, touchOpts);
      viewport.removeEventListener("touchend", onTouchEnd, touchOpts);
      viewport.removeEventListener("touchcancel", onTouchEnd, touchOpts);
      viewport.removeEventListener("click", onClickCapture, true);
      viewport.removeEventListener("mousedown", onMouseDownCapture, {
        capture: true,
      });
      window.removeEventListener("mouseup", onWindowMouseUp, { capture: true });
      viewport.removeEventListener("wheel", onWheelCapture, true);
      xtermTextarea?.removeEventListener("beforeinput", onBeforeInput, {
        capture: true,
      });
      if (wheelPersistTimer) clearTimeout(wheelPersistTimer);
      if (resizeDebounceTimer) clearTimeout(resizeDebounceTimer);
      if (fontSizeRaf !== null) cancelAnimationFrame(fontSizeRaf);
      if (timingPingTimer) clearInterval(timingPingTimer);
      if (timingSummaryTimer) clearInterval(timingSummaryTimer);
      if (timing) {
        const w = window as Window & { __aoeTiming?: TerminalTiming };
        if (w.__aoeTiming === timing) delete w.__aoeTiming;
      }
      // Detach handlers BEFORE closing so the soon-to-fire onclose closure
      // can't schedule a retry that races the next session's connect path.
      // Without this, the old onclose runs after cleanup, calls setTimeout
      // -> connect() in the captured (now stale) sessionId scope, and
      // overwrites wsRef.current with a ghost socket for the previous
      // session. See #1455.
      const owned = wsRef.current;
      if (owned) {
        owned.onopen = null;
        owned.onmessage = null;
        owned.onclose = null;
        owned.onerror = null;
        owned.close();
      }
      term.dispose();
      if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
      if (countdownRef.current) clearInterval(countdownRef.current);
      termRef.current = null;
      fitRef.current = null;
      wsRef.current = null;
    };
  }, [sessionId, terminalSetState, wsPath]);

  // Repaint the live terminal when the user picks a new theme. The
  // resolved theme's --term-* variables live on documentElement; xterm.js
  // doesn't watch them, so we re-read the palette and reassign
  // term.options.theme to swap it live.
  useEffect(() => {
    const onThemeChanged = () => {
      const term = termRef.current;
      if (!term) return;
      term.options.theme = readThemeFromCss();
    };
    window.addEventListener("aoe:theme-changed", onThemeChanged);
    return () => {
      window.removeEventListener("aoe:theme-changed", onThemeChanged);
    };
  }, []);

  // Apply font size changes from settings UI to the live terminal.
  useEffect(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term) return;
    const size = window.innerWidth < MOBILE_BREAKPOINT_PX ? settings.mobileFontSize : settings.desktopFontSize;
    if (term.options.fontSize !== size) {
      term.options.fontSize = size;
      try {
        fit?.fit();
      } catch {
        // ignore
      }
    }
  }, [settings.mobileFontSize, settings.desktopFontSize]);

  // Mirror state.isInScrollback into a ref so the resize callback can read
  // the latest value, and drain any pending deferred resize when the user
  // exits scrollback (so claude redraws once at the final size).
  useEffect(() => {
    const wasInScrollback = isInScrollbackRef.current;
    isInScrollbackRef.current = state.isInScrollback;
    if (wasInScrollback && !state.isInScrollback) {
      setTimeout(() => flushPendingResizeRef.current?.(), 0);
    }
  }, [state.isInScrollback]);

  const manualReconnect = () => {
    const ws = wsRef.current;
    tdbg("manualReconnect()", {
      readyState: ws?.readyState,
      previousAttempt: retryCountRef.current,
    });
    // Cancel any armed backoff retry / countdown so the upcoming connect
    // isn't immediately followed by the scheduled one.
    if (retryTimerRef.current) {
      clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    if (countdownRef.current) {
      clearInterval(countdownRef.current);
      countdownRef.current = null;
    }
    retryCountRef.current = 0;
    terminalSetState((prev) => ({
      ...prev,
      connected: false,
      reconnecting: true,
      retryCount: 0,
      retryCountdown: 0,
    }));
    // If the socket is already CLOSED, ws.close() is a no-op and no
    // onclose will fire to drive the retry path; dial a fresh socket
    // directly. CONNECTING / OPEN / CLOSING all still have onclose
    // ahead of them, so close() + onclose's retry handler is the
    // right path there.
    if (!ws || ws.readyState === WebSocket.CLOSED) {
      connectRef.current?.();
    } else {
      ws.close();
    }
  };
  useEffect(() => {
    manualReconnectRef.current = manualReconnect;
    return () => {
      if (manualReconnectRef.current === manualReconnect) {
        manualReconnectRef.current = null;
      }
    };
  });

  const sendData = useCallback((data: string) => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(new TextEncoder().encode(data));
    }
  }, []);

  const activate = useCallback(() => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify({ type: "activate" } as ActivateMessage));
    }
  }, []);

  // Mobile-only: sends ESC to force tmux out of copy-mode. On mobile we
  // clamp scroll-down so tmux never reaches the bottom on its own; the
  // button is the only way back to live.
  //
  // Also sends `resume_output` so the server SIGCONTs the pane's
  // process tree (which was paused on entry to scrollback). The server
  // auto-resumes on disconnect as a safety net, so forgetting this is
  // annoying but not permanent.
  const exitScrollback = useCallback(() => {
    // Cancel any in-flight momentum decay first. Otherwise a tap that
    // lands while a fast flick is still emitting wheel-ups would let the
    // next decay frame call sendWheel("up", ...), which re-sets
    // isInScrollback: true and the button reappears.
    cancelMomentumRef.current?.();
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "resume_output" } as ResumeOutputMessage));
      ws.send(new TextEncoder().encode("\x1b"));
    }
    resetScrollbackDepthRef.current?.();
    terminalSetState((prev) => (prev.isInScrollback ? { ...prev, isInScrollback: false } : prev));
  }, [terminalSetState]);

  return {
    containerRef,
    termRef,
    state,
    manualReconnect,
    sendData,
    activate,
    exitScrollback,
    ctrlActiveRef,
    clearCtrlRef,
    maxRetries: MAX_RETRIES,
  };
}
