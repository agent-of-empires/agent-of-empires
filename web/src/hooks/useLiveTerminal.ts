import { useCallback, useEffect, useEffectEvent, useRef } from "react";
import { useSnapshotStore } from "./useSnapshotStore";
import { listen } from "./domEvents";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { getToken } from "../lib/token";
import { buttonMouseBytes, wheelMouseBytes } from "../lib/liveMouse";
import { createFrameInflater, supportsFrameDeflate, type FrameInflater } from "../lib/frameStream";
import { MAX_RETRIES, retryDelayMs } from "../lib/wsBackoff";
import { reportTelemetrySeen } from "../lib/api";

// Mirrors CLOSE_CODE_PTY_DEAD in src/server/pane.rs.
const CLOSE_CODE_PTY_DEAD = 4001;
const MAX_PENDING_INPUT_BYTES = 64 * 1024;

export interface LiveCursor {
  x: number;
  y: number;
}

export interface LivePaneRect {
  cols: number;
  rows: number;
  left?: number;
  top?: number;
}

export interface LiveStats {
  frames: number;
  patches: number;
  wireBytes: number;
  resyncs: number;
}

export interface LiveFrame {
  content: string;
  lines?: string[];
  seq?: number;
  receivedAt?: number;
  rows: number;
  history: number;
  cursor: LiveCursor | null;
  altScreen: boolean;
  mouse: boolean;
  mouseSgr: boolean;
  pane0?: LivePaneRect | null;
}

export interface LiveTerminalState {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  frame: LiveFrame | null;
  reading: boolean;
  isOwner: boolean;
  ownerKnown: boolean;
  transport: "grid" | "snapshot" | null;
  stats: LiveStats;
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
  transport: null,
  stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
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
  const desiredRef = useRef<{
    resize: { cols: number; rows: number } | null;
    window: number | null;
    fast: boolean;
  }>({ resize: null, window: null, fast: true });
  const readingRef = useRef(false);
  const telemetrySeenRef = useRef(false);
  const pendingInputRef = useRef<Uint8Array<ArrayBuffer>[]>([]);
  // Hold input until the server confirms this connection owns the pane.
  const ownerKnownRef = useRef(false);

  const { state, read, setState } = useSnapshotStore(() => INITIAL_STATE);

  const sendIfOpen = useCallback((data: string | ArrayBufferView<ArrayBuffer>) => {
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) ws.send(data);
  }, []);

  const setWindowInternal = useCallback(
    (lines: number) => {
      if (desiredRef.current.window === lines) return;
      desiredRef.current.window = lines;
      sendIfOpen(JSON.stringify({ type: "window", lines }));
    },
    [sendIfOpen],
  );

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
    const stats: LiveStats = { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 };
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
      const url = wsPath.startsWith("/")
        ? `${proto}//${location.host}${wsPath}`
        : `${proto}//${location.host}/sessions/${sessionId}/${wsPath}`;
      const token = getToken();
      let bindingSecret: string | null = null;
      try {
        bindingSecret = getOrCreateDeviceBindingSecret();
      } catch {
        // Storage or crypto unavailable; let the server reject.
      }
      const protocols: string[] = ["aoe-auth"];
      if (token) protocols.push(token);
      if (bindingSecret) protocols.push(`aoe-device.${bindingSecret}`);
      const ws = new WebSocket(url, protocols);
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
        ws.send(JSON.stringify({ type: "claim_if_vacant" }));
        const desired = desiredRef.current;
        if (desired.resize) {
          ws.send(JSON.stringify({ type: "resize", ...desired.resize }));
        }
        if (desired.window != null) {
          ws.send(JSON.stringify({ type: "window", lines: desired.window }));
        }
        ws.send(JSON.stringify({ type: "cadence", fast: desired.fast }));
        ws.send(JSON.stringify({ type: "caps", deflate: supportsFrameDeflate(), patch: true }));
      };

      let hasReceivedData = false;
      let lastSeq: number | null = null;
      let resyncPending = false;
      const handleMessageText = (text: string) => {
        if (wsRef.current !== ws) return;
        let msg: {
          type?: string;
          grid?: boolean;
          content?: string;
          seq?: number;
          base?: number;
          shift?: number;
          lines?: [number, string][];
          text?: string;
          rows?: number;
          history?: number;
          cursor?: LiveCursor | null;
          is_owner?: boolean;
          altScreen?: boolean;
          mouse?: boolean;
          mouseSgr?: boolean;
          pane0?: LivePaneRect | null;
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
        if (msg.type === "transport") {
          const transport = msg.grid ? "grid" : "snapshot";
          setState((prev) => (prev.transport === transport ? prev : { ...prev, transport }));
          return;
        }
        if (msg.type === "clipboard") {
          if (typeof msg.text !== "string" || msg.text.length === 0) return;
          handleClipboard(msg.text);
          return;
        }
        if (msg.type !== "frame" && msg.type !== "patch") return;
        if (!hasReceivedData) {
          hasReceivedData = true;
          retryCountRef.current = 0;
        }
        let content: string;
        let lines: string[];
        if (msg.type === "patch") {
          const prev = read().frame;
          if (prev?.lines == null || lastSeq == null || msg.base !== lastSeq) {
            if (!resyncPending) {
              resyncPending = true;
              stats.resyncs += 1;
              ws.send(JSON.stringify({ type: "resync" }));
            }
            return;
          }
          lines = applyPatch(prev.lines, msg.shift ?? 0, msg.lines ?? []);
          content = lines.join("\n") + "\n";
          stats.patches += 1;
        } else {
          content = msg.content ?? "";
          lines = frameLines(content);
          resyncPending = false;
          stats.frames += 1;
        }
        lastSeq = msg.seq ?? null;
        const incoming: LiveFrame = {
          content,
          lines,
          seq: msg.seq,
          receivedAt: performance.now(),
          rows: msg.rows ?? 0,
          history: msg.history ?? 0,
          cursor: msg.cursor ?? null,
          altScreen: msg.altScreen ?? false,
          mouse: msg.mouse ?? false,
          mouseSgr: msg.mouseSgr ?? false,
          pane0: msg.pane0 ?? null,
        };
        // Keep the capture window covering the full history while reading, or old lines fall out.
        if (readingRef.current) {
          const full = Math.min(4000, incoming.rows + incoming.history);
          if (full > (desiredRef.current.window ?? 0)) setWindowInternal(full);
        }
        setState((prev) => ({
          ...prev,
          retryCount: retryCountRef.current,
          retryCountdown: 0,
          frame: incoming,
          stats: { ...stats },
        }));
      };

      ws.onmessage = (event: MessageEvent) => {
        if (wsRef.current !== ws) return;
        if (typeof event.data === "string") {
          stats.wireBytes += event.data.length;
          handleMessageText(event.data);
        } else if (event.data instanceof ArrayBuffer) {
          stats.wireBytes += event.data.byteLength;
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
    const stopVisibility = listen(onVisibility, [document, "visibilitychange"]);
    const stopNetwork = listen(tryAutoReconnect, [window, "online"], [window, "pageshow"]);

    return () => {
      disposed = true;
      disposeInflater();
      stopVisibility();
      stopNetwork();
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
  }, [sessionId, wsPath, setState, read, setWindowInternal]);

  const typedWordRef = useRef("");

  const sendData = useCallback(
    (data: string): boolean => {
      typedWordRef.current = "";
      const ws = wsRef.current;
      const isOwner = read().isOwner;
      if (ownerKnownRef.current && isOwner && ws?.readyState === WebSocket.OPEN) {
        ws.send(new TextEncoder().encode(data));
        return true;
      }
      // A confirmed non-owner must not queue keystrokes for a later takeover.
      if (ownerKnownRef.current && !isOwner) return false;
      const bytes = new TextEncoder().encode(data);
      const pending = pendingInputRef.current;
      const used = pending.reduce((total, item) => total + item.byteLength, 0);
      if (bytes.byteLength > MAX_PENDING_INPUT_BYTES - used) return false;
      pending.push(bytes);
      return true;
    },
    [read],
  );

  const claim = useCallback(() => sendIfOpen(JSON.stringify({ type: "claim" })), [sendIfOpen]);

  const forwardWheel = useCallback(
    (up: boolean, sgr: boolean, col: number, row: number) => sendIfOpen(wheelMouseBytes(up, sgr, col, row)),
    [sendIfOpen],
  );

  const forwardButton = useCallback(
    (baseButton: number, release: boolean, motion: boolean, sgr: boolean, col: number, row: number) =>
      sendIfOpen(buttonMouseBytes(baseButton, release, motion, sgr, col, row)),
    [sendIfOpen],
  );

  const sendResize = useCallback(
    (cols: number, rows: number) => {
      const prev = desiredRef.current.resize;
      if (prev && prev.cols === cols && prev.rows === rows) return;
      desiredRef.current.resize = { cols, rows };
      sendIfOpen(JSON.stringify({ type: "resize", cols, rows }));
    },
    [sendIfOpen],
  );

  const setWindow = useCallback((lines: number) => setWindowInternal(lines), [setWindowInternal]);

  const setCadence = useCallback(
    (fast: boolean) => {
      if (desiredRef.current.fast === fast) return;
      desiredRef.current.fast = fast;
      sendIfOpen(JSON.stringify({ type: "cadence", fast }));
    },
    [sendIfOpen],
  );

  const enterReading = useCallback(
    (rows: number) => {
      if (readingRef.current) return;
      readingRef.current = true;
      const latest = read().frame;
      const full = Math.min(4000, Math.max(rows, latest ? latest.rows + latest.history : rows));
      setWindowInternal(full);
      setState((prev) => ({ ...prev, reading: true }));
    },
    [read, setState, setWindowInternal],
  );

  const returnToLive = useCallback(
    (rows: number) => {
      if (!readingRef.current) return;
      readingRef.current = false;
      if (rows > 0) setWindowInternal(rows);
      setState((prev) => ({ ...prev, reading: false }));
    },
    [setState, setWindowInternal],
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
    typedWordRef,
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

export function frameLines(content: string): string[] {
  const lines = content.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

export function applyPatch(prev: readonly string[], shift: number, changed: readonly [number, string][]): string[] {
  const n = prev.length;
  const k = Math.max(0, Math.min(n, Math.trunc(shift)));
  const next = prev.slice(k).concat(prev.slice(0, k).map(() => ""));
  for (const [i, row] of changed) {
    if (i >= 0 && i < n) next[i] = row;
  }
  return next;
}
