import { useCallback, useEffect, useRef, useSyncExternalStore } from "react";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { getToken } from "../lib/token";
import { retryDelayMs } from "./useTerminal";

// Capture-snapshot live view transport (mobile). Mirrors the TUI's
// live-send model: the server polls `tmux capture-pane` and pushes ANSI
// snapshot frames; we send raw input bytes back, plus control messages
// for resize / capture-window / cadence. No xterm, no PTY attach; the
// component renders frames as DOM text and scrolls natively. See
// src/server/live_ws.rs for the protocol.

const MAX_RETRIES = 7;
/** Mirrors CLOSE_CODE_PTY_DEAD in src/server/ws.rs. */
const CLOSE_CODE_PTY_DEAD = 4001;

export interface LiveCursor {
  x: number;
  y: number;
}

export interface LiveFrame {
  content: string;
  /** Pane height in rows; the content's last `rows` lines are the live
   *  screen. 0 if the pane geometry probe failed. */
  rows: number;
  /** Lines currently in tmux scrollback; sizes the client's virtual
   *  scroll spacer. */
  history: number;
  /** Cursor cell, or null when hidden (DECTCEM off) or unavailable. */
  cursor: LiveCursor | null;
}

export interface LiveTerminalState {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  /** Frame to RENDER. While reading scrollback this is the frozen
   *  full-history snapshot; at the live edge it tracks the stream. */
  frame: LiveFrame | null;
  /** True from the moment the user leaves the live edge until they
   *  return: drives the jump-to-latest affordance. */
  reading: boolean;
}

const INITIAL_STATE: LiveTerminalState = {
  connected: false,
  reconnecting: false,
  retryCount: 0,
  retryCountdown: 0,
  frame: null,
  reading: false,
};

/** Cheap line count for the freeze trigger; capture content terminates
 *  every line with `\n`, so count terminators. */
function contentLineCount(content: string): number {
  let n = 0;
  for (let i = 0; i < content.length; i++) if (content.charCodeAt(i) === 10) n++;
  return n;
}

export function useLiveTerminal(sessionId: string | null, wsPath: string = "live-ws") {
  const wsRef = useRef<WebSocket | null>(null);
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const countdownRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const retryCountRef = useRef(0);
  const connectRef = useRef<(() => void) | null>(null);
  // Latest resize/window/cadence the component asked for, re-sent on
  // (re)connect so a fresh server-side handler picks up where the old
  // one left off.
  const desiredRef = useRef<{
    resize: { cols: number; rows: number } | null;
    window: number | null;
    fast: boolean;
    hold: boolean;
  }>({ resize: null, window: null, fast: true, hold: false });
  // Scrollback read machine (see LiveTerminalState.reading). "fetching"
  // means the full-history window was requested and the next covering
  // frame will be frozen; "held" means the server push-freeze is active.
  const readPhaseRef = useRef<"live" | "fetching" | "held">("live");
  // Freeze threshold: the requested window, capped by what the pane can
  // actually provide (rows + history at request time).
  const fetchTargetRef = useRef(0);
  // Newest frame from the wire, regardless of freeze state, so returning
  // to the live edge can repaint instantly while a fresh frame arrives.
  const latestFrameRef = useRef<LiveFrame | null>(null);

  const storeRef = useRef<{
    snapshot: LiveTerminalState;
    listeners: Set<() => void>;
  } | null>(null);
  if (storeRef.current == null) {
    storeRef.current = { snapshot: INITIAL_STATE, listeners: new Set() };
  }
  const setState = useCallback((fn: (prev: LiveTerminalState) => LiveTerminalState) => {
    const store = storeRef.current!;
    store.snapshot = fn(store.snapshot);
    store.listeners.forEach((l) => l());
  }, []);
  const subscribe = useCallback((listener: () => void) => {
    storeRef.current!.listeners.add(listener);
    return () => {
      storeRef.current!.listeners.delete(listener);
    };
  }, []);
  const getSnapshot = useCallback(() => storeRef.current!.snapshot, []);
  const state = useSyncExternalStore(subscribe, getSnapshot);

  useEffect(() => {
    if (!sessionId) return;

    wsRef.current?.close();
    if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
    if (countdownRef.current) clearInterval(countdownRef.current);
    retryCountRef.current = 0;
    setState(() => INITIAL_STATE);

    let disposed = false;

    function connect() {
      if (disposed) return;
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      const url = `${proto}//${location.host}/sessions/${sessionId}/${wsPath}`;
      const token = getToken();
      let bindingSecret: string | null = null;
      try {
        bindingSecret = getOrCreateDeviceBindingSecret();
      } catch {
        // Storage/crypto unavailable; let the server reject.
      }
      const protocols: string[] = ["aoe-auth"];
      if (token) protocols.push(token);
      if (bindingSecret) protocols.push(`aoe-device.${bindingSecret}`);
      const ws = new WebSocket(url, protocols);
      wsRef.current = ws;

      ws.onopen = () => {
        setState((prev) => ({
          ...prev,
          connected: true,
          reconnecting: false,
        }));
        // Replay the component's desired geometry so a reconnected
        // server-side handler matches the client immediately.
        const desired = desiredRef.current;
        if (desired.resize) {
          ws.send(JSON.stringify({ type: "resize", ...desired.resize }));
        }
        if (desired.window != null) {
          ws.send(JSON.stringify({ type: "window", lines: desired.window }));
        }
        ws.send(JSON.stringify({ type: "cadence", fast: desired.fast }));
        if (desired.hold) {
          ws.send(JSON.stringify({ type: "hold", hold: true }));
        }
      };

      let hasReceivedData = false;
      ws.onmessage = (event: MessageEvent) => {
        if (typeof event.data !== "string") return;
        let msg: {
          type?: string;
          content?: string;
          rows?: number;
          history?: number;
          cursor?: LiveCursor | null;
        };
        try {
          msg = JSON.parse(event.data) as typeof msg;
        } catch {
          return;
        }
        if (msg.type !== "frame") return;
        if (!hasReceivedData) {
          // First frame proves the capture loop is alive end-to-end;
          // only now reset the retry budget (mirrors useTerminal).
          hasReceivedData = true;
          retryCountRef.current = 0;
        }
        const incoming: LiveFrame = {
          content: msg.content ?? "",
          rows: msg.rows ?? 0,
          history: msg.history ?? 0,
          cursor: msg.cursor ?? null,
        };
        latestFrameRef.current = incoming;
        if (readPhaseRef.current === "held") {
          // Frozen: the rendered frame must not move under the reader.
          // (The server holds pushes anyway; this guards stragglers.)
          setState((prev) => ({
            ...prev,
            retryCount: retryCountRef.current,
            retryCountdown: 0,
          }));
          return;
        }
        if (
          readPhaseRef.current === "fetching" &&
          contentLineCount(incoming.content) >= Math.min(fetchTargetRef.current, incoming.rows + incoming.history)
        ) {
          // This frame covers the requested history: freeze it and stop
          // the server's pushes until the reader returns to the edge.
          readPhaseRef.current = "held";
          desiredRef.current.hold = true;
          ws.send(JSON.stringify({ type: "hold", hold: true }));
        }
        setState((prev) => ({
          ...prev,
          retryCount: retryCountRef.current,
          retryCountdown: 0,
          frame: incoming,
        }));
      };

      ws.onclose = (event: CloseEvent) => {
        if (disposed) return;
        setState((prev) => ({ ...prev, connected: false }));
        if (event.code === CLOSE_CODE_PTY_DEAD) {
          retryCountRef.current = MAX_RETRIES;
        }
        if (retryCountRef.current < MAX_RETRIES) {
          retryCountRef.current += 1;
          const count = retryCountRef.current;
          const delayMs = retryDelayMs(count);
          let countdown = Math.ceil(delayMs / 1000);
          setState((prev) => ({
            ...prev,
            reconnecting: true,
            retryCount: count,
            retryCountdown: countdown,
          }));
          countdownRef.current = setInterval(() => {
            countdown -= 1;
            if (countdown > 0) {
              setState((prev) => ({ ...prev, retryCountdown: countdown }));
            }
          }, 1000);
          retryTimerRef.current = setTimeout(() => {
            if (countdownRef.current) clearInterval(countdownRef.current);
            connect();
          }, delayMs);
        } else {
          setState((prev) => ({
            ...prev,
            reconnecting: false,
            retryCount: retryCountRef.current,
            retryCountdown: 0,
          }));
        }
      };
    }
    connectRef.current = connect;
    connect();

    // Wake-from-suspend recovery: iOS can drop the socket without a
    // delivered onclose while the PWA is backgrounded. Redial when the
    // page becomes visible / regains network and the socket is gone.
    const tryAutoReconnect = () => {
      const readyState = wsRef.current?.readyState;
      if (readyState === WebSocket.OPEN || readyState === WebSocket.CONNECTING) return;
      if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
      if (countdownRef.current) clearInterval(countdownRef.current);
      retryCountRef.current = 0;
      connect();
    };
    const onVisibility = () => {
      if (document.visibilityState === "visible") tryAutoReconnect();
    };
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("online", tryAutoReconnect);
    window.addEventListener("pageshow", tryAutoReconnect);

    return () => {
      disposed = true;
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("online", tryAutoReconnect);
      window.removeEventListener("pageshow", tryAutoReconnect);
      if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
      if (countdownRef.current) clearInterval(countdownRef.current);
      const ws = wsRef.current;
      if (ws) {
        ws.onopen = null;
        ws.onmessage = null;
        ws.onclose = null;
        ws.close();
      }
      wsRef.current = null;
      connectRef.current = null;
    };
  }, [sessionId, wsPath, setState]);

  const sendData = useCallback((data: string) => {
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(new TextEncoder().encode(data));
    }
  }, []);

  const sendResize = useCallback((cols: number, rows: number) => {
    // Dedup: the sizing observer recomputes on every container change,
    // but rows are latched to the no-keyboard height, so keyboard cycles
    // arrive here with identical dimensions and must not touch tmux.
    const prev = desiredRef.current.resize;
    if (prev && prev.cols === cols && prev.rows === rows) return;
    desiredRef.current.resize = { cols, rows };
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "resize", cols, rows }));
    }
  }, []);

  const setWindowInternal = (lines: number) => {
    if (desiredRef.current.window === lines) return;
    desiredRef.current.window = lines;
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "window", lines }));
    }
  };
  const setWindow = useCallback((lines: number) => {
    setWindowInternal(lines);
  }, []);

  const setCadence = useCallback((fast: boolean) => {
    if (desiredRef.current.fast === fast) return;
    desiredRef.current.fast = fast;
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "cadence", fast }));
    }
  }, []);

  /** The user left the live edge: request the full history once; the
   *  covering frame freezes and the server push-freezes (see onmessage). */
  const enterReading = useCallback(
    (rows: number) => {
      if (readPhaseRef.current !== "live") return;
      const latest = latestFrameRef.current;
      const full = Math.min(4000, Math.max(rows, latest ? latest.rows + latest.history : rows));
      readPhaseRef.current = "fetching";
      fetchTargetRef.current = full;
      setWindowInternal(full);
      setState((prev) => ({ ...prev, reading: true }));
    },
    [setState],
  );

  /** Back at the live edge: release the hold, shrink the window, and
   *  resume rendering the freshest frame immediately. */
  const returnToLive = useCallback(
    (rows: number) => {
      if (readPhaseRef.current === "live") return;
      readPhaseRef.current = "live";
      desiredRef.current.hold = false;
      const ws = wsRef.current;
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "hold", hold: false }));
      }
      if (rows > 0) setWindowInternal(rows);
      setState((prev) => ({ ...prev, reading: false, frame: latestFrameRef.current ?? prev.frame }));
    },
    [setState],
  );

  const manualReconnect = useCallback(() => {
    if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
    if (countdownRef.current) clearInterval(countdownRef.current);
    retryCountRef.current = 0;
    setState((prev) => ({
      ...prev,
      connected: false,
      reconnecting: true,
      retryCount: 0,
      retryCountdown: 0,
    }));
    const ws = wsRef.current;
    if (!ws || ws.readyState === WebSocket.CLOSED) {
      connectRef.current?.();
    } else {
      ws.close();
    }
  }, [setState]);

  return {
    state,
    sendData,
    sendResize,
    setWindow,
    setCadence,
    enterReading,
    returnToLive,
    manualReconnect,
    maxRetries: MAX_RETRIES,
  };
}
