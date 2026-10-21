import { useCallback, useEffect, useRef } from "react";
import { useLatestRef } from "./useLatestRef";
import type { SessionResponse } from "../lib/types";
import { approvalChimedRecently, playSoundFor } from "../lib/soundPlayer";
import { nextStatusSoundState, soundDueFor, type StatusSoundEvent, type StatusSoundState } from "../lib/statusSounds";

// Server cues for several sessions arrive as separate frames; one sound per event covers a burst.
const COALESCE_MS = 500;

/** Plays the waiting, idle and error sounds for every session, from each session list refresh. */
export function useStatusSounds(sessions: readonly SessionResponse[]): (id: string, event: StatusSoundEvent) => void {
  const stateRef = useRef<StatusSoundState | null>(null);
  const sessionsRef = useLatestRef(sessions);
  const lastPlayedRef = useRef(new Map<StatusSoundEvent, number>());
  // Cues rejected against a list that had not caught up yet; the server will not resend them.
  const pendingRef = useRef(new Map<string, StatusSoundEvent>());

  const playOnce = useCallback((id: string, event: StatusSoundEvent, now: number) => {
    if (event === "waiting" && approvalChimedRecently(id, now)) return;
    const last = lastPlayedRef.current.get(event);
    if (last !== undefined && now - last < COALESCE_MS) return;
    lastPlayedRef.current.set(event, now);
    void playSoundFor(event);
  }, []);

  useEffect(() => {
    const now = Date.now();
    const { state, due } = nextStatusSoundState(stateRef.current, sessions, now);
    stateRef.current = state;
    for (const d of due) playOnce(d.id, d.event, now);

    const pending = pendingRef.current;
    for (const [id, event] of pending) {
      const folded = soundDueFor(
        stateRef.current,
        sessions.find((x) => x.id === id),
        event,
      );
      stateRef.current = folded.state;
      if (folded.play) playOnce(id, event, now);
    }
    pending.clear();
  }, [sessions, playOnce]);

  // The server times the dwell, so a cue sounds without waiting for a session-list refresh.
  return useCallback(
    (id: string, event: StatusSoundEvent) => {
      const session = sessionsRef.current.find((x) => x.id === id);
      const { state, play } = soundDueFor(stateRef.current, session, event);
      stateRef.current = state;
      if (play) playOnce(id, event, Date.now());
      else pendingRef.current.set(id, event);
    },
    [sessionsRef, playOnce],
  );
}
