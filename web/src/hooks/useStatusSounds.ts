import { useCallback, useEffect, useRef } from "react";
import { useLatestRef } from "./useLatestRef";
import type { SessionResponse, SessionStatus } from "../lib/types";
import { approvalChimedRecently, playSoundFor } from "../lib/soundPlayer";
import { nextStatusSoundState, soundDueFor, type StatusSoundEvent, type StatusSoundState } from "../lib/statusSounds";

// Server cues for several sessions arrive as separate frames; one sound per event covers a burst.
const COALESCE_MS = 500;

/** Plays the waiting, idle and error sounds for every session, from each session list refresh. */
export function useStatusSounds(sessions: readonly SessionResponse[]): (id: string, event: StatusSoundEvent) => void {
  const stateRef = useRef<StatusSoundState | null>(null);
  const sessionsRef = useLatestRef(sessions);
  const lastPlayedRef = useRef(new Map<StatusSoundEvent, number>());
  // Cues rejected against a list that had not caught up yet, with the status it still showed; the server will
  // not resend them. A cue lives until the list moves past that status.
  const pendingRef = useRef(new Map<string, { event: StatusSoundEvent; stale: SessionStatus }>());

  const playOnce = useCallback((id: string, event: StatusSoundEvent, now: number) => {
    if (event === "waiting" && approvalChimedRecently(id, now)) return;
    const last = lastPlayedRef.current.get(event);
    if (last !== undefined && now - last < COALESCE_MS) return;
    lastPlayedRef.current.set(event, now);
    void playSoundFor(event);
  }, []);

  const replayPending = useCallback(
    (list: readonly SessionResponse[], now: number) => {
      const pending = pendingRef.current;
      for (const [id, { event, stale }] of pending) {
        const session = list.find((x) => x.id === id);
        const folded = soundDueFor(stateRef.current, session, event);
        stateRef.current = folded.state;
        if (folded.play) playOnce(id, event, now);
        if (folded.play || session?.status !== stale) pending.delete(id);
      }
    },
    [playOnce],
  );

  useEffect(() => {
    const now = Date.now();
    const { state, due } = nextStatusSoundState(stateRef.current, sessions, now);
    stateRef.current = state;
    for (const d of due) playOnce(d.id, d.event, now);

    replayPending(sessions, now);
  }, [sessions, playOnce, replayPending]);

  // The server times the dwell, so a cue sounds without waiting for a session-list refresh.
  return useCallback(
    (id: string, event: StatusSoundEvent) => {
      const session = sessionsRef.current.find((x) => x.id === id);
      const { state, play } = soundDueFor(stateRef.current, session, event);
      stateRef.current = state;
      if (play) playOnce(id, event, Date.now());
      else if (session) pendingRef.current.set(id, { event, stale: session.status });
    },
    [sessionsRef, playOnce],
  );
}
