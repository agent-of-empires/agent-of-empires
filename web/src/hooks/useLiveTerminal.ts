import { useCallback, useEffect, useEffectEvent, useLayoutEffect, useRef } from "react";
import { useSnapshotStore } from "./useSnapshotStore";
import { listen } from "./domEvents";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { getToken } from "../lib/token";
import { buttonMouseBytes, wheelMouseBytes } from "../lib/liveMouse";
import { createFrameInflater, supportsFrameDeflate, type FrameInflater } from "../lib/frameStream";
import { MAX_RETRIES, retryDelayMs } from "../lib/wsBackoff";
import { reportTelemetrySeen } from "../lib/api";
import type { LiveClientMessage, LivePane0, LiveServerMessage } from "../lib/generated/liveWire";

export type { LiveCursor, LivePane0 as LivePaneRect } from "../lib/generated/liveWire";

const encodeControl = (message: LiveClientMessage) => JSON.stringify(message);

// Mirrors CLOSE_CODE_PTY_DEAD in src/server/pane.rs.
const CLOSE_CODE_PTY_DEAD = 4001;
const MAX_PENDING_INPUT_BYTES = 64 * 1024;

export interface LiveStats {
  frames: number;
  patches: number;
  wireBytes: number;
  resyncs: number;
}

type FrameMessage = Extract<LiveServerMessage, { type: "frame" }>;

export type LiveFrame = Omit<FrameMessage, "type" | "seq" | "pane0"> & {
  lines?: string[];
  seq?: number;
  receivedAt?: number;
  pane0?: LivePane0 | null;
};

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
  /** Why the server is on the snapshot fallback, when it said. */
  transportReason: "pipe_owned" | null;
  /** The agent exited and left a dead pane; redialing cannot help until it is restarted. */
  paneDead: boolean;
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
  transportReason: null,
  paneDead: false,
  stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
};

export function useLiveTerminal(
  sessionId: string | null,
  readOnly: boolean,
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
  // Binary keystrokes and JSON paste messages, in send order.
  const pendingInputRef = useRef<(Uint8Array<ArrayBuffer> | string)[]>([]);
  // Hold input until the server confirms this connection owns the pane.
  const ownerKnownRef = useRef(false);
  const readOnlyRef = useRef(readOnly);
  useLayoutEffect(() => {
    readOnlyRef.current = readOnly;
    if (readOnly) {
      pendingInputRef.current = [];
      desiredRef.current.resize = null;
    }
  }, [readOnly]);

  const { state, read, setState } = useSnapshotStore(() => INITIAL_STATE);

  const sendIfOpen = useCallback((data: string | ArrayBufferView<ArrayBuffer>) => {
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) ws.send(data);
  }, []);

  const sendMutationIfOpen = useCallback(
    (data: string | ArrayBufferView<ArrayBuffer>) => {
      if (!readOnlyRef.current) sendIfOpen(data);
    },
    [sendIfOpen],
  );

  const setWindowInternal = useCallback(
    (lines: number) => {
      if (desiredRef.current.window === lines) return;
      desiredRef.current.window = lines;
      sendIfOpen(encodeControl({ type: "window", lines }));
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
        if (readOnlyRef.current || wsRef.current !== ws || !ownerKnownRef.current || ws.readyState !== WebSocket.OPEN)
          return;
        const pending = pendingInputRef.current;
        pendingInputRef.current = [];
        for (const data of pending) ws.send(data);
      };

      ws.onopen = () => {
        if (wsRef.current !== ws) return;
        if (!readOnlyRef.current && !telemetrySeenRef.current) {
          telemetrySeenRef.current = true;
          reportTelemetrySeen("web_terminal");
        }
        setState((prev) => ({
          ...prev,
          connected: true,
          reconnecting: false,
          paneDead: false,
        }));
        const desired = desiredRef.current;
        if (!readOnlyRef.current) {
          ws.send(encodeControl({ type: "claim_if_vacant" }));
          if (desired.resize) ws.send(encodeControl({ type: "resize", ...desired.resize }));
        }
        if (desired.window != null) {
          ws.send(encodeControl({ type: "window", lines: desired.window }));
        }
        ws.send(encodeControl({ type: "cadence", fast: desired.fast }));
        ws.send(encodeControl({ type: "caps", deflate: supportsFrameDeflate(), patch: true }));
      };

      let hasReceivedData = false;
      let lastSeq: number | null = null;
      let resyncPending = false;
      const handleMessageText = (text: string) => {
        if (wsRef.current !== ws) return;
        let msg: LiveServerMessage;
        try {
          msg = JSON.parse(text) as LiveServerMessage;
        } catch {
          return;
        }
        if (msg.type === "size_owner") {
          const owner = msg.is_owner;
          ownerKnownRef.current = true;
          setState((prev) =>
            prev.isOwner === owner && prev.ownerKnown ? prev : { ...prev, isOwner: owner, ownerKnown: true },
          );
          if (owner) flushPendingInput();
          return;
        }
        if (msg.type === "transport") {
          const transport = msg.grid ? "grid" : "snapshot";
          const transportReason = msg.grid ? null : (msg.reason ?? null);
          setState((prev) =>
            prev.transport === transport && prev.transportReason === transportReason
              ? prev
              : { ...prev, transport, transportReason },
          );
          return;
        }
        if (msg.type === "clipboard") {
          if (msg.text.length === 0) return;
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
              ws.send(encodeControl({ type: "resync" }));
            }
            return;
          }
          lines = applyPatch(prev.lines, msg.shift, msg.lines);
          content = lines.join("\n") + "\n";
          stats.patches += 1;
        } else {
          content = msg.content;
          lines = frameLines(content);
          resyncPending = false;
          stats.frames += 1;
        }
        lastSeq = msg.seq;
        const incoming: LiveFrame = {
          content,
          lines,
          seq: msg.seq,
          receivedAt: performance.now(),
          rows: msg.rows,
          history: msg.history,
          cursor: msg.cursor,
          altScreen: msg.altScreen,
          mouse: msg.mouse,
          mouseSgr: msg.mouseSgr,
          pane0: msg.pane0,
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
        const paneDead = event.code === CLOSE_CODE_PTY_DEAD;
        setState((prev) => ({ ...prev, connected: false, isOwner: false, ownerKnown: false, paneDead }));
        if (paneDead) {
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

  /** Whether `message` was sent or queued for the owner confirmation. */
  const sendInput = useCallback(
    (message: Uint8Array<ArrayBuffer> | string): boolean => {
      if (readOnlyRef.current) return false;
      const ws = wsRef.current;
      const isOwner = read().isOwner;
      if (ownerKnownRef.current && isOwner && ws?.readyState === WebSocket.OPEN) {
        ws.send(message);
        return true;
      }
      // A confirmed non-owner must not queue keystrokes for a later takeover.
      if (ownerKnownRef.current && !isOwner) return false;
      const size = (item: Uint8Array | string) =>
        typeof item === "string" ? new TextEncoder().encode(item).byteLength : item.byteLength;
      const pending = pendingInputRef.current;
      const used = pending.reduce((total, item) => total + size(item), 0);
      if (size(message) > MAX_PENDING_INPUT_BYTES - used) return false;
      pending.push(message);
      return true;
    },
    [read],
  );

  const sendData = useCallback((data: string) => sendInput(new TextEncoder().encode(data)), [sendInput]);

  /** tmux pastes `text` (bracketed only if the pane asked for it), then presses Enter when `submit`. */
  const sendPaste = useCallback(
    (text: string, submit: boolean) => sendInput(encodeControl({ type: "paste", text, submit })),
    [sendInput],
  );

  const claim = useCallback(() => sendMutationIfOpen(encodeControl({ type: "claim" })), [sendMutationIfOpen]);

  const forwardWheel = useCallback(
    (up: boolean, sgr: boolean, col: number, row: number) => sendMutationIfOpen(wheelMouseBytes(up, sgr, col, row)),
    [sendMutationIfOpen],
  );

  const forwardButton = useCallback(
    (baseButton: number, release: boolean, motion: boolean, sgr: boolean, col: number, row: number) =>
      sendMutationIfOpen(buttonMouseBytes(baseButton, release, motion, sgr, col, row)),
    [sendMutationIfOpen],
  );

  const sendResize = useCallback(
    (cols: number, rows: number) => {
      if (readOnlyRef.current) return;
      const prev = desiredRef.current.resize;
      if (prev && prev.cols === cols && prev.rows === rows) return;
      desiredRef.current.resize = { cols, rows };
      sendIfOpen(encodeControl({ type: "resize", cols, rows }));
    },
    [sendIfOpen],
  );

  const setWindow = useCallback((lines: number) => setWindowInternal(lines), [setWindowInternal]);

  const setCadence = useCallback(
    (fast: boolean) => {
      if (desiredRef.current.fast === fast) return;
      desiredRef.current.fast = fast;
      sendIfOpen(encodeControl({ type: "cadence", fast }));
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
    sendPaste,
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
