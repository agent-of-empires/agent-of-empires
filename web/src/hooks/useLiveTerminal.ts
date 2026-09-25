import { useCallback, useEffect, useEffectEvent, useRef } from "react";
import { useSnapshotStore } from "./useSnapshotStore";
import { listen } from "./domEvents";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { getToken } from "../lib/token";
import { buttonMouseBytes } from "../lib/liveMouse";
import { createFrameInflater, supportsFrameDeflate, type FrameInflater } from "../lib/frameStream";
import type { LiveClientMessage, LiveCursor, LivePane0, LiveServerMessage } from "../lib/liveWire";
import { MAX_RETRIES, retryDelayMs } from "../lib/wsBackoff";
import { reportTelemetrySeen } from "../lib/api";

// Mirrors CLOSE_CODE_PTY_DEAD in src/server/pane.rs.
const CLOSE_CODE_PTY_DEAD = 4001;
const MAX_PENDING_INPUT_BYTES = 64 * 1024;
/** Used until the daemon announces its own ceiling on the `transport`
 * message. Only a floor for the first window request, since the daemon
 * clamps whatever it is asked for. */
const DEFAULT_MAX_WINDOW = 4000;

/** Encodes a control message. The type is the point: `LiveClientMessage` is
 *  generated from src/daemon/live.rs, so a renamed or added field fails here
 *  rather than being ignored on the wire. */
function controlPayload(msg: LiveClientMessage): string {
  return JSON.stringify(msg);
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
  /** App is in SGR (1006) mouse encoding. The daemon encodes forwarded
   *  wheel notches with it; the button path still encodes here. */
  mouseSgr: boolean;
  /** App is in any-event tracking (1003), so it wants bare motion reports
   *  and a client can forward hover. */
  mouseAll: boolean;
  /** Pane 0's rectangle within the composited window grid (a split
   *  window). */
  pane0?: LivePane0 | null;
}

export interface LiveTerminalState {
  connected: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  frame: LiveFrame | null;
  reading: boolean;
  isOwner: boolean;
  /** Who holds the size lock instead of this client, when the daemon named
   *  them. Null while this client owns it, or when the lock is simply free. */
  holder: string | null;
  /** The server has answered this connection's initial size-owner request.
   * Until then input is buffered and the UI must not present a takeover
   * banner as though another viewer had been confirmed. */
  ownerKnown: boolean;
  transport: "grid" | "snapshot" | null;
  /** The capture window ceiling the daemon clamps to, once it has said.
   *  Asking for more is harmless (the daemon clamps anyway); this just keeps
   *  the client from duplicating the constant. */
  maxWindow: number;
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
  holder: null,
  ownerKnown: false,
  transport: null,
  maxWindow: DEFAULT_MAX_WINDOW,
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
      sendIfOpen(controlPayload({ type: "window", lines }));
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
        // Negotiate ownership even if mobile keyboard occlusion has deferred
        // the first safe resize. The server claims only a vacant lock here;
        // an explicit takeover remains a separate user action.
        ws.send(controlPayload({ type: "claim_if_vacant" }));
        // Replay the component's desired geometry so a reconnected
        // server-side handler matches the client immediately.
        const desired = desiredRef.current;
        if (desired.resize) {
          ws.send(controlPayload({ type: "resize", ...desired.resize }));
        }
        if (desired.window != null) {
          ws.send(controlPayload({ type: "window", lines: desired.window }));
        }
        ws.send(controlPayload({ type: "cadence", fast: desired.fast }));
        // Advertise row patches, and the compressed frame stream where the
        // browser can inflate it; the server keeps sending full JSON text
        // frames otherwise (and old servers ignore the unknown message type).
        ws.send(controlPayload({ type: "caps", deflate: supportsFrameDeflate(), patch: true }));
        // Preserve the selector's gesture-bound first burst, but do not flush
        // it yet: the resize that establishes size ownership may still be in
        // flight, especially when a native TUI currently owns the pane.
      };

      let hasReceivedData = false;
      let lastSeq: number | null = null;
      let resyncPending = false;
      const handleMessageText = (text: string) => {
        if (wsRef.current !== ws) return;
        // `LiveServerMessage` is generated from src/daemon/live.rs, so a
        // field added there arrives here. The cast is still a claim about a
        // peer we do not control: read defensively below.
        let msg: LiveServerMessage;
        try {
          msg = JSON.parse(text) as LiveServerMessage;
        } catch {
          return;
        }
        if (msg.type === "size_owner") {
          const owner = msg.is_owner ?? true;
          const holder = owner ? null : (msg.holder ?? null);
          ownerKnownRef.current = true;
          setState((prev) =>
            prev.isOwner === owner && prev.ownerKnown && prev.holder === holder
              ? prev
              : { ...prev, isOwner: owner, holder, ownerKnown: true },
          );
          if (owner) flushPendingInput();
          return;
        }
        if (msg.type === "transport") {
          const transport = msg.grid ? "grid" : "snapshot";
          const maxWindow = msg.maxWindow ?? DEFAULT_MAX_WINDOW;
          setState((prev) =>
            prev.transport === transport && prev.maxWindow === maxWindow ? prev : { ...prev, transport, maxWindow },
          );
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
              ws.send(controlPayload({ type: "resync" }));
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
          seq: msg.seq ?? undefined,
          receivedAt: performance.now(),
          rows: msg.rows ?? 0,
          history: msg.history ?? 0,
          cursor: msg.cursor ?? null,
          altScreen: msg.altScreen ?? false,
          mouse: msg.mouse ?? false,
          mouseSgr: msg.mouseSgr ?? false,
          mouseAll: msg.mouseAll ?? false,
          pane0: msg.pane0 ?? null,
        };
        // Keep the capture window covering the full history while reading, or old lines fall out.
        if (readingRef.current) {
          const full = Math.min(read().maxWindow, incoming.rows + incoming.history);
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

  /** True when the pane will receive `data`: sent now, or queued for a flush
   *  that is still expected. False means it was dropped and no caller may
   *  treat it as delivered. */
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

  /** Explicit take-over from a read-only viewer: steal the size-owner lock
   *  even from a live holder, then size the window to this client. */
  const claim = useCallback(() => sendIfOpen(controlPayload({ type: "claim" })), [sendIfOpen]);

  /** Forward wheel notches to a full-screen app (alternate screen). NOT a
   *  window request: the alternate screen has no capturable scrollback, so the
   *  app scrolls its own content and the next frame reflects it.
   *
   *  Sent as a control message rather than raw input bytes, so the daemon
   *  encodes it against the pane's live modes. Raw input is dropped for a
   *  client that does not hold the size lock, which left a watcher unable to
   *  scroll at all; the daemon takes this from any viewer that is not
   *  read-only. `count` coalesces a gesture's notches into one message. */
  const forwardWheel = useCallback(
    (up: boolean, col: number, row: number, count = 1) => {
      if (count <= 0) return;
      sendIfOpen(
        controlPayload({
          type: "wheel",
          up,
          col: Math.max(1, Math.floor(col)),
          row: Math.max(1, Math.floor(row)),
          count,
        }),
      );
    },
    [sendIfOpen],
  );

  /** Forward a mouse button press/drag/release to a full-screen mouse app,
   *  encoded as the app expects. Raw input bytes, unlike the wheel, so this
   *  one still needs the size lock. */
  const forwardButton = useCallback(
    (baseButton: number, release: boolean, motion: boolean, sgr: boolean, col: number, row: number) =>
      sendIfOpen(buttonMouseBytes(baseButton, release, motion, sgr, col, row)),
    [sendIfOpen],
  );

  const sendResize = useCallback(
    (cols: number, rows: number) => {
      // Dedup: the sizing observer recomputes on every container change, but
      // rows are latched to the no-keyboard height, so keyboard cycles arrive
      // here with identical dimensions and must not touch tmux.
      const prev = desiredRef.current.resize;
      if (prev && prev.cols === cols && prev.rows === rows) return;
      desiredRef.current.resize = { cols, rows };
      sendIfOpen(controlPayload({ type: "resize", cols, rows }));
    },
    [sendIfOpen],
  );

  const setWindow = useCallback((lines: number) => setWindowInternal(lines), [setWindowInternal]);

  const setCadence = useCallback(
    (fast: boolean) => {
      if (desiredRef.current.fast === fast) return;
      desiredRef.current.fast = fast;
      sendIfOpen(controlPayload({ type: "cadence", fast }));
    },
    [sendIfOpen],
  );

  const enterReading = useCallback(
    (rows: number) => {
      if (readingRef.current) return;
      readingRef.current = true;
      const snapshot = read();
      const latest = snapshot.frame;
      const full = Math.min(snapshot.maxWindow, Math.max(rows, latest ? latest.rows + latest.history : rows));
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
