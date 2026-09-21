// ACP WebSocket lifecycle: replay catch-up, backoff reconnect, liveness watchdog, and older-history paging.

import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type Dispatch, type RefObject } from "react";
import {
  emptyAcpState,
  type AcpFrame,
  type AcpState,
  type ReducedState,
  type TranscriptDelta,
  type TranscriptRow,
} from "../../lib/acpTypes";
import { getOrCreateDeviceBindingSecret } from "../../lib/deviceBinding";
import { getToken } from "../../lib/token";
import { listen } from "../domEvents";
import { useLatestRef } from "../useLatestRef";
import { toActivityRows, transcriptDeltaAction, type Action } from "./reducer";
import { fetchOlderPage, fetchReplay } from "./replay";
import { cacheGet } from "./stateCache";

export type ConnectionStatus = "connecting" | "open" | "closed" | "error";

export const ACP_MAX_RETRIES = 7;
const ACP_RETRY_BASE_MS = 1000;
const ACP_RETRY_CAP_MS = 30000;
/** Backoff 1s, 2s, 4s, 8s, 16s, then 30s. */
export function acpRetryDelayMs(attempt: number): number {
  return Math.min(ACP_RETRY_CAP_MS, ACP_RETRY_BASE_MS * 2 ** Math.max(0, attempt - 1));
}

// The server sends a heartbeat every 30s. A proxy RST can leave a socket OPEN but dead, so a
// socket silent past this window is redialed. It stays under the daemon's 90s pong reaper.
const ACP_WS_WATCHDOG_INTERVAL_MS = 15000;
export const ACP_WS_STALE_MS = 75000;

type ServerMessage =
  | AcpFrame
  | { kind: "lagged"; skipped?: number }
  | { kind: "heartbeat" }
  | { kind: "reduced_state"; state?: ReducedState; unchanged?: string[] }
  | { kind: "transcript_snapshot"; rows?: TranscriptRow[] }
  | { kind: "transcript_delta"; delta?: TranscriptDelta };

function closeQuietly(ws: WebSocket): void {
  try {
    ws.close();
  } catch {
    // Already closed.
  }
}

const subscribeVisibility = (cb: () => void) => listen(cb, [document, "visibilitychange"], [window, "pageshow"]);
const subscribeOnline = (cb: () => void) => listen(cb, [window, "online"], [window, "offline"]);

function acpSocketProtocols(): string[] {
  const token = getToken();
  let bindingSecret: string | null = null;
  try {
    bindingSecret = getOrCreateDeviceBindingSecret();
  } catch {
    // Storage or crypto unavailable; the server rejects the upgrade and login surfaces why.
  }
  const protocols = ["aoe-auth"];
  if (token) protocols.push(token);
  if (bindingSecret) protocols.push(`aoe-device.${bindingSecret}`);
  return protocols;
}

export function useAcpConnection(
  sessionId: string | null,
  sessionIdRef: RefObject<string | null>,
  state: AcpState,
  dispatch: Dispatch<Action>,
) {
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [reconnecting, setReconnecting] = useState(false);
  const [retryCount, setRetryCount] = useState(0);
  const [retryCountdown, setRetryCountdown] = useState(0);
  const [hasMoreOlder, setHasMoreOlder] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [hasEverOpened, setHasEverOpened] = useState(false);

  const lastSeqRef = useLatestRef(state.lastSeq);
  const oldestSeqRef = useLatestRef(state.oldestSeq);
  const hasMoreOlderRef = useLatestRef(hasMoreOlder);
  const loadingOlderRef = useRef(false);
  const wsRef = useRef<WebSocket | null>(null);
  const retryCountRef = useRef(0);
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const countdownTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const connectRef = useRef<(() => void) | null>(null);
  // Bumped per dial; handlers of a superseded dial must not touch the current socket.
  const dialGenRef = useRef(0);
  const lastServerMsgRef = useRef(0);
  // Last applied frame or submit, polled by the force-end-turn affordance without re-rendering.
  const lastActivityRef = useRef(0);
  // Setters behind a ref: the socket handlers below are not an external-store subscription.
  const settersRef = useRef({ setStatus, setReconnecting, setRetryCount, setRetryCountdown, setHasEverOpened });

  const clearRetryTimers = useCallback(() => {
    if (retryTimerRef.current) {
      clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    if (countdownTimerRef.current) {
      clearInterval(countdownTimerRef.current);
      countdownTimerRef.current = null;
    }
  }, []);

  const redial = useCallback(() => {
    retryCountRef.current = 0;
    setRetryCount(0);
    setRetryCountdown(0);
    clearRetryTimers();
    connectRef.current?.();
  }, [clearRetryTimers]);

  const tryAutoReconnect = useCallback(() => {
    const ready = wsRef.current?.readyState;
    if (ready === WebSocket.CONNECTING) return;
    // An OPEN socket only counts as alive while it keeps hearing from the server.
    if (ready === WebSocket.OPEN && Date.now() - lastServerMsgRef.current < ACP_WS_STALE_MS) return;
    redial();
  }, [redial]);

  useEffect(() => {
    const id = setInterval(() => {
      if (wsRef.current?.readyState === WebSocket.OPEN) tryAutoReconnect();
    }, ACP_WS_WATCHDOG_INTERVAL_MS);
    return () => clearInterval(id);
  }, [tryAutoReconnect]);

  const visCounterRef = useRef(0);
  const subscribeVisibilityCount = useCallback(
    (cb: () => void) =>
      subscribeVisibility(() => {
        visCounterRef.current += 1;
        cb();
      }),
    [],
  );
  const visCounter = useSyncExternalStore(
    subscribeVisibilityCount,
    () => visCounterRef.current,
    () => 0,
  );
  const isOnline = useSyncExternalStore(
    subscribeOnline,
    () => navigator.onLine,
    () => true,
  );

  const isFirstVis = useRef(true);
  useEffect(() => {
    if (isFirstVis.current) {
      isFirstVis.current = false;
      return;
    }
    tryAutoReconnect();
  }, [visCounter, tryAutoReconnect]);

  const prevOnlineRef = useRef(isOnline);
  useEffect(() => {
    if (!prevOnlineRef.current && isOnline) tryAutoReconnect();
    prevOnlineRef.current = isOnline;
  }, [isOnline, tryAutoReconnect]);

  const loadOlder = useCallback(async () => {
    const sid = sessionIdRef.current;
    const before = oldestSeqRef.current;
    if (!sid || before <= 0 || loadingOlderRef.current || !hasMoreOlderRef.current) return;
    loadingOlderRef.current = true;
    setLoadingOlder(true);
    try {
      const more = await fetchOlderPage(sid, before, dispatch);
      if (more !== null) setHasMoreOlder(more);
    } catch {
      // Keep hasMoreOlder; the next scroll-up retries.
    } finally {
      loadingOlderRef.current = false;
      setLoadingOlder(false);
    }
  }, [dispatch, sessionIdRef, oldestSeqRef, hasMoreOlderRef]);

  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);
  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    setStatus(sessionId ? "connecting" : "closed");
    setReconnecting(false);
    setRetryCount(0);
    setRetryCountdown(0);
    setHasMoreOlder(false);
    setLoadingOlder(false);
    setHasEverOpened(false);
  }

  useEffect(() => {
    const cached = sessionId ? cacheGet(sessionId) : undefined;
    loadingOlderRef.current = false;
    lastSeqRef.current = cached?.lastSeq ?? 0;
    oldestSeqRef.current = cached?.oldestSeq ?? 0;
    if (!sessionId) return;
    dispatch({ kind: "hydrate", state: cached ?? emptyAcpState() });
    retryCountRef.current = 0;
    let cancelled = false;
    const { setStatus, setReconnecting, setRetryCount, setRetryCountdown, setHasEverOpened } = settersRef.current;

    const scheduleReconnect = () => {
      if (cancelled) return;
      if (retryCountRef.current >= ACP_MAX_RETRIES) {
        setReconnecting(false);
        setRetryCount(retryCountRef.current);
        setRetryCountdown(0);
        return;
      }
      const attempt = ++retryCountRef.current;
      const delayMs = acpRetryDelayMs(attempt);
      let countdown = Math.ceil(delayMs / 1000);
      setReconnecting(true);
      setRetryCount(attempt);
      setRetryCountdown(countdown);
      clearRetryTimers();
      countdownTimerRef.current = setInterval(() => {
        countdown -= 1;
        if (countdown > 0) setRetryCountdown(countdown);
      }, 1000);
      retryTimerRef.current = setTimeout(() => {
        if (countdownTimerRef.current) {
          clearInterval(countdownTimerRef.current);
          countdownTimerRef.current = null;
        }
        connectRef.current?.();
      }, delayMs);
    };

    const handleMessage = (data: ServerMessage) => {
      const kind = typeof data === "object" && data !== null && "kind" in data ? data.kind : undefined;
      switch (kind) {
        case "heartbeat":
          return;
        case "lagged":
          dispatch({ kind: "lagged", skipped: (data as { skipped?: number }).skipped ?? 0 });
          void fetchReplay(sessionId, lastSeqRef, dispatch, setHasMoreOlder);
          return;
        case "reduced_state": {
          const { state: reduced, unchanged } = data as { state?: ReducedState; unchanged?: string[] };
          if (!reduced) return;
          lastActivityRef.current = Date.now();
          dispatch({ kind: "reduced_state", state: reduced, unchanged: unchanged ?? [] });
          return;
        }
        case "transcript_snapshot": {
          const rows = toActivityRows((data as { rows?: TranscriptRow[] }).rows ?? [], sessionId);
          lastActivityRef.current = Date.now();
          dispatch({ kind: "transcript_snapshot", rows });
          return;
        }
        case "transcript_delta": {
          const delta = (data as { delta?: TranscriptDelta }).delta;
          const act = delta ? transcriptDeltaAction(delta, sessionId) : null;
          if (!act) return;
          lastActivityRef.current = Date.now();
          dispatch(act);
          return;
        }
      }
      if (typeof data === "object" && data !== null && "session_id" in data && "event" in data) {
        lastActivityRef.current = Date.now();
        dispatch({ kind: "frame", frame: data as AcpFrame });
      }
    };

    const connect = () => {
      if (cancelled) return;
      clearRetryTimers();
      dialGenRef.current += 1;
      if (wsRef.current) {
        closeQuietly(wsRef.current);
        wsRef.current = null;
      }
      const myGen = dialGenRef.current;
      const isCurrentDial = () => !cancelled && dialGenRef.current === myGen;
      void (async () => {
        await fetchReplay(sessionId, lastSeqRef, dispatch, setHasMoreOlder);
        if (!isCurrentDial()) return;
        const protocol = window.location.protocol === "https:" ? "wss" : "ws";
        const url = `${protocol}://${window.location.host}/sessions/${encodeURIComponent(sessionId)}/acp/ws?since=${lastSeqRef.current}`;
        const ws = new WebSocket(url, acpSocketProtocols());
        wsRef.current = ws;
        ws.onopen = () => {
          if (!isCurrentDial()) {
            closeQuietly(ws);
            return;
          }
          setStatus("open");
          setHasEverOpened(true);
          lastServerMsgRef.current = Date.now();
          retryCountRef.current = 0;
          setReconnecting(false);
          setRetryCount(0);
          setRetryCountdown(0);
        };
        ws.onerror = () => {
          if (isCurrentDial()) setStatus("error");
        };
        ws.onclose = () => {
          if (!isCurrentDial()) return;
          setStatus("closed");
          wsRef.current = null;
          scheduleReconnect();
        };
        ws.onmessage = (ev) => {
          if (!isCurrentDial()) return;
          lastServerMsgRef.current = Date.now();
          try {
            handleMessage(JSON.parse(ev.data) as ServerMessage);
          } catch {
            // Ignore malformed frames.
          }
        };
      })();
    };
    connectRef.current = connect;
    connect();

    return () => {
      cancelled = true;
      dialGenRef.current += 1;
      clearRetryTimers();
      if (wsRef.current) closeQuietly(wsRef.current);
      wsRef.current = null;
      connectRef.current = null;
    };
  }, [sessionId, dispatch, clearRetryTimers, lastSeqRef, oldestSeqRef]);

  const manualReconnect = useCallback(() => {
    setReconnecting(false);
    redial();
  }, [redial]);

  return {
    status,
    reconnecting,
    retryCount,
    retryCountdown,
    manualReconnect,
    hasEverOpened,
    loadOlder,
    hasMoreOlder,
    loadingOlder,
    lastActivityRef,
  };
}
