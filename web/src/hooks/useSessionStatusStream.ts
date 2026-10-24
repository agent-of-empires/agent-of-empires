import { useEffect } from "react";
import type { RuntimeCursor } from "../lib/api";
import type { SessionResponse } from "../lib/types";
import type { StatusSoundEvent } from "../lib/statusSounds";
import { retryDelayMs } from "../lib/wsBackoff";
import { acpSocketProtocols } from "./acpSession/useAcpConnection";
import { useLatestRef } from "./useLatestRef";

export interface SessionStatusStreamHandlers {
  /** `cursor` is the snapshot that already includes the move. */
  onStatus: (id: string, status: SessionResponse["status"], cursor: RuntimeCursor) => void;
  onSoundDue: (id: string, event: StatusSoundEvent) => void;
  /** The stream is (re)established or lost events; refetch the full list. */
  onResync: () => void;
}

const SOUND_EVENTS: readonly string[] = ["waiting", "idle", "error"];

/** Applies server-pushed status moves as they happen; the polled list stays authoritative for everything else. */
export function useSessionStatusStream(handlers: SessionStatusStreamHandlers): void {
  const handlersRef = useLatestRef(handlers);

  useEffect(() => {
    let ws: WebSocket | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    let stopped = false;

    const connect = () => {
      const protocol = window.location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(`${protocol}://${window.location.host}/api/sessions/status/ws`, acpSocketProtocols());
      ws.onmessage = (e) => {
        if (stopped) return;
        let frame: { kind?: string; id?: string; status?: string; event?: string; epoch?: string; revision?: string };
        try {
          frame = JSON.parse(String(e.data));
        } catch {
          return;
        }
        const h = handlersRef.current;
        if (frame.kind === "ready") {
          attempt = 0;
          h.onResync();
        } else if (frame.kind === "resync") {
          h.onResync();
        } else if (
          frame.kind === "status" &&
          frame.id &&
          frame.status &&
          frame.epoch &&
          /^\d+$/.test(frame.revision ?? "")
        ) {
          h.onStatus(frame.id, frame.status as SessionResponse["status"], {
            epoch: frame.epoch,
            revision: BigInt(frame.revision!),
          });
        } else if (frame.kind === "sound_due" && frame.id && SOUND_EVENTS.includes(frame.event ?? "")) {
          h.onSoundDue(frame.id, frame.event as StatusSoundEvent);
        }
      };
      ws.onclose = () => {
        if (stopped) return;
        timer = setTimeout(connect, retryDelayMs(++attempt));
      };
    };

    connect();
    return () => {
      stopped = true;
      clearTimeout(timer);
      ws?.close();
    };
  }, [handlersRef]);
}
