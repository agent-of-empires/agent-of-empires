import { useCallback, useEffect, useEffectEvent, useRef, useSyncExternalStore } from "react";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { getToken } from "../lib/token";
import { buttonMouseBytes, wheelMouseBytes } from "../lib/liveMouse";
import { createFrameInflater, supportsFrameDeflate, type FrameInflater } from "../lib/frameStream";
import { MAX_RETRIES, retryDelayMs } from "../lib/wsBackoff";
import { reportTelemetrySeen } from "../lib/api";

// Capture-snapshot live view transport (mobile). Mirrors the TUI's
// live-send model: the server polls `tmux capture-pane` and pushes ANSI
// snapshot frames; we send raw input bytes back, plus control messages
// for resize / capture-window / cadence. No xterm, no PTY attach; the
// component renders frames as DOM text and scrolls natively. See
// src/server/live_ws.rs for the protocol. When the browser supports it,
// the client advertises `caps.deflate` and frames arrive as a compressed
// binary stream (lib/frameStream.ts) instead of JSON text; both paths
// feed the same handler.

/** Mirrors CLOSE_CODE_PTY_DEAD in src/server/pane.rs. */
const CLOSE_CODE_PTY_DEAD = 4001;
/** Keep a short burst typed while a newly selected session's socket opens.
 * The cap prevents an offline tab from retaining unbounded paste data. */
const MAX_PENDING_INPUT_BYTES = 64 * 1024;

export interface LiveCursor {
  x: number;
  y: number;
}

export interface LivePaneExtent {
  cols: number;
  rows: number;
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
  /** Pane is on the alternate screen (a full-screen / TUI app). Its
   *  scrollback is not capturable, so scroll gestures forward the wheel
   *  to the app instead of widening the capture window. */
  altScreen: boolean;
  /** App has some mouse tracking mode on (it will consume forwarded wheel
   *  events). Forwarding only happens when this AND altScreen are set. */
  mouse: boolean;
  /** App is in SGR (1006) mouse encoding; picks the forwarded wire format
   *  (SGR vs legacy X10). */
  mouseSgr: boolean;
  /** Pane 0's input extent when the frame composites a split window. */
  pane0?: LivePaneExtent | null;
}

export interface LiveTerminalState {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  /** Frame to RENDER. Always tracks the stream (the agent keeps running
   *  while you read, like the TUI's live mode); reading scrollback just
   *  asks for a bigger capture window. */
  frame: LiveFrame | null;
  /** True from the moment the user leaves the live edge until they
   *  return: widens the capture window and drives the jump-to-latest
   *  affordance. */
  reading: boolean;
  /** Whether this client holds the session's size-owner lock and may
   *  resize/type. Only one client at a time owns it across every surface
   *  (web PTY attach, mobile live view, native TUI); a non-owner renders
   *  best-effort at the owner's grid and shows a "take over" banner. */
  isOwner: boolean;
  /** The server has answered this connection's initial size-owner request.
   * Until then input is buffered and the UI must not present a takeover
   * banner as though another viewer had been confirmed. */
  ownerKnown: boolean;
}

const INITIAL_STATE: LiveTerminalState = {
  connected: false,
  reconnecting: false,
  retryCount: 0,
  retryCountdown: 0,
  frame: null,
  reading: false,
  isOwner: false,
  ownerKnown: false,
};

export function useLiveTerminal(
  sessionId: string | null,
  wsPath: string = "live-ws",
  onClipboard?: (text: string) => void,
) {
  const handleClipboard = useEffectEvent((text: string) => onClipboard?.(text));
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
  }>({ resize: null, window: null, fast: true });
  // Whether the user is reading scrollback (off the live edge). Guards
  // enterReading/returnToLive against repeat fires from scroll events.
  const readingRef = useRef(false);
  // Fire the `web_terminal` usage signal once per hook lifetime, not on every
  // reconnect: onopen runs again after a WiFi blip, and the telemetry intent is
  // "this terminal was opened", not "the socket reconnected N times". Ported
  // from the removed xterm useTerminal hook.
  const telemetrySeenRef = useRef(false);
  const pendingInputRef = useRef<Uint8Array<ArrayBuffer>[]>([]);
  // A resize and the resulting size-owner result are ordered on the socket,
  // but React has not necessarily rendered that result when a mobile user
  // types their first key. Hold that burst until the server confirms this
  // connection owns the pane instead of relying on a speculative default.
  const ownerKnownRef = useRef(false);

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

  // Declared ahead of the connect effect: the onmessage handler widens
  // the window while reading (see below).
  const setWindowInternal = (lines: number) => {
    if (desiredRef.current.window === lines) return;
    desiredRef.current.window = lines;
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "window", lines }));
    }
  };

  useEffect(() => {
    if (!sessionId) {
      pendingInputRef.current = [];
      ownerKnownRef.current = false;
      return;
    }

    wsRef.current?.close();
    pendingInputRef.current = [];
    ownerKnownRef.current = false;
    if (retryTimerRef.current) clearTimeout(retryTimerRef.current);
    if (countdownRef.current) clearInterval(countdownRef.current);
    retryCountRef.current = 0;
    setState(() => INITIAL_STATE);

    let disposed = false;
    // Inflater for the compressed frame stream, one per live connection
    // (its deflate dictionary is connection-scoped on the server side).
    let inflater: FrameInflater | null = null;
    const disposeInflater = () => {
      inflater?.dispose();
      inflater = null;
    };

    function connect() {
      if (disposed) return;
      disposeInflater();
      ownerKnownRef.current = false;
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      // A leading-slash `wsPath` is an absolute relay path; otherwise it is a
      // per-session suffix under `/sessions/<id>/`.
      const url = wsPath.startsWith("/")
        ? `${proto}//${location.host}${wsPath}`
        : `${proto}//${location.host}/sessions/${sessionId}/${wsPath}`;
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
      // Compressed frames arrive as binary; ArrayBuffer avoids the async
      // Blob-read hop on every message.
      ws.binaryType = "arraybuffer";
      wsRef.current = ws;

      const flushPendingInput = () => {
        if (wsRef.current !== ws || !ownerKnownRef.current || ws.readyState !== WebSocket.OPEN) return;
        const pending = pendingInputRef.current;
        pendingInputRef.current = [];
        for (const data of pending) ws.send(data);
      };

      ws.onopen = () => {
        if (wsRef.current !== ws) return;
        if (!telemetrySeenRef.current) {
          telemetrySeenRef.current = true;
          reportTelemetrySeen("web_terminal");
        }
        setState((prev) => ({
          ...prev,
          connected: true,
          reconnecting: false,
        }));
        // Negotiate ownership even if mobile keyboard occlusion has deferred
        // the first safe resize. The server claims only a vacant lock here;
        // an explicit takeover remains a separate user action.
        ws.send(JSON.stringify({ type: "claim_if_vacant" }));
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
        // Advertise the compressed frame stream where the browser can
        // inflate it; the server keeps sending JSON text otherwise (and
        // old servers ignore the unknown message type).
        if (supportsFrameDeflate()) {
          ws.send(JSON.stringify({ type: "caps", deflate: true }));
        }
        // Preserve the selector's gesture-bound first burst, but do not flush
        // it yet: the resize that establishes size ownership may still be in
        // flight, especially when a native TUI currently owns the pane.
      };

      let hasReceivedData = false;
      const handleMessageText = (text: string) => {
        if (wsRef.current !== ws) return;
        let msg: {
          type?: string;
          content?: string;
          text?: string;
          rows?: number;
          history?: number;
          cursor?: LiveCursor | null;
          is_owner?: boolean;
          altScreen?: boolean;
          mouse?: boolean;
          mouseSgr?: boolean;
          pane0?: LivePaneExtent | null;
        };
        try {
          msg = JSON.parse(text) as typeof msg;
        } catch {
          return;
        }
        if (msg.type === "size_owner") {
          const owner = msg.is_owner ?? true;
          ownerKnownRef.current = true;
          setState((prev) =>
            prev.isOwner === owner && prev.ownerKnown ? prev : { ...prev, isOwner: owner, ownerKnown: true },
          );
          if (owner) flushPendingInput();
          return;
        }
        if (msg.type === "clipboard") {
          if (typeof msg.text !== "string" || msg.text.length === 0) return;
          handleClipboard(msg.text);
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
          altScreen: msg.altScreen ?? false,
          mouse: msg.mouse ?? false,
          mouseSgr: msg.mouseSgr ?? false,
          pane0: msg.pane0 ?? null,
        };
        // While reading, keep the capture window covering the FULL
        // history as the agent appends: the window was sized at entry,
        // so without this the oldest lines fall out of the capture and
        // re-render as blank spacer under the reader. Deduped, so it is
        // one control message per growth step at idle cadence.
        if (readingRef.current) {
          const full = Math.min(4000, incoming.rows + incoming.history);
          if (full > (desiredRef.current.window ?? 0)) setWindowInternal(full);
        }
        // Always render the freshest frame. While reading scrollback the
        // window is wider, but the component's spacer model keeps the
        // user's position stable as the agent streams (above-viewport
        // pixels are invariant), so no freeze is needed.
        setState((prev) => ({
          ...prev,
          retryCount: retryCountRef.current,
          retryCountdown: 0,
          frame: incoming,
        }));
      };

      ws.onmessage = (event: MessageEvent) => {
        if (wsRef.current !== ws) return;
        if (typeof event.data === "string") {
          handleMessageText(event.data);
        } else if (event.data instanceof ArrayBuffer) {
          // Compressed frame stream, built lazily on the first binary
          // message. A corrupt stream is unrecoverable mid-connection, so
          // its error path drops the socket and lets the retry machinery
          // redial (fresh dictionary on both sides).
          inflater ??= createFrameInflater(handleMessageText, () => ws.close());
          inflater.push(event.data);
        }
      };

      ws.onclose = (event: CloseEvent) => {
        if (disposed || wsRef.current !== ws) return;
        disposeInflater();
        ownerKnownRef.current = false;
        setState((prev) => ({ ...prev, connected: false, isOwner: false, ownerKnown: false }));
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
      disposeInflater();
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
    const canSend = ownerKnownRef.current && storeRef.current!.snapshot.isOwner;
    if (canSend && ws?.readyState === WebSocket.OPEN) {
      ws.send(new TextEncoder().encode(data));
      return;
    }
    // A confirmed non-owner must not leave keystrokes queued for a later
    // takeover. Only the short, unresolved initial handshake (or a socket
    // reconnect while we were the owner) may retain input.
    if (ownerKnownRef.current && !storeRef.current!.snapshot.isOwner) return;
    const bytes = new TextEncoder().encode(data);
    const pending = pendingInputRef.current;
    const used = pending.reduce((total, item) => total + item.byteLength, 0);
    if (bytes.byteLength <= MAX_PENDING_INPUT_BYTES - used) {
      pending.push(bytes);
    }
  }, []);

  /** Explicit take-over from a read-only viewer: steal the size-owner lock
   *  even from a live holder, then size the window to this client. */
  const claim = useCallback(() => {
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "claim" }));
    }
  }, []);

  /** Forward a wheel notch to a full-screen mouse app (alternate screen),
   *  encoded as the app expects. Sent as raw input bytes, NOT as a window
   *  request: the alternate screen has no capturable scrollback, so the
   *  app scrolls its own content and the next frame reflects it. */
  const forwardWheel = useCallback((up: boolean, sgr: boolean, col: number, row: number) => {
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(wheelMouseBytes(up, sgr, col, row));
    }
  }, []);

  /** Forward a mouse button press/drag/release to a full-screen mouse app,
   *  encoded as the app expects. Sent as raw input bytes on the same path as
   *  the wheel; the app reacts and the next frame reflects it. */
  const forwardButton = useCallback(
    (baseButton: number, release: boolean, motion: boolean, sgr: boolean, col: number, row: number) => {
      const ws = wsRef.current;
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(buttonMouseBytes(baseButton, release, motion, sgr, col, row));
      }
    },
    [],
  );

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

  /** The user left the live edge: widen the capture window to the full
   *  history once so a flick lands on real content (the spacer is
   *  already sized for it). The stream keeps flowing; the component's
   *  spacer keeps the reading position stable. */
  const enterReading = useCallback(
    (rows: number) => {
      if (readingRef.current) return;
      readingRef.current = true;
      const latest = storeRef.current!.snapshot.frame;
      const full = Math.min(4000, Math.max(rows, latest ? latest.rows + latest.history : rows));
      setWindowInternal(full);
      setState((prev) => ({ ...prev, reading: true }));
    },
    [setState],
  );

  /** Back at the live edge: shrink the window to the live screen so the
   *  next frame is small again. */
  const returnToLive = useCallback(
    (rows: number) => {
      if (!readingRef.current) return;
      readingRef.current = false;
      if (rows > 0) setWindowInternal(rows);
      setState((prev) => ({ ...prev, reading: false }));
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
    forwardWheel,
    forwardButton,
    sendResize,
    setWindow,
    setCadence,
    enterReading,
    returnToLive,
    manualReconnect,
    claim,
    maxRetries: MAX_RETRIES,
  };
}
