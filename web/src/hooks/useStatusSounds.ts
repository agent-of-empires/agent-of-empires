import { useCallback, useEffect, useRef } from "react";
import { useLatestRef } from "./useLatestRef";
import type { SessionResponse } from "../lib/types";
import { approvalChimedRecently, playSoundFor } from "../lib/soundPlayer";
import { nextStatusSoundState, soundDueFor, type StatusSoundEvent, type StatusSoundState } from "../lib/statusSounds";

/** Plays the waiting, idle and error sounds for every session, from each session list refresh. */
export function useStatusSounds(sessions: readonly SessionResponse[]): (id: string, event: StatusSoundEvent) => void {
  const stateRef = useRef<StatusSoundState | null>(null);
  const sessionsRef = useLatestRef(sessions);

  useEffect(() => {
    const now = Date.now();
    const { state, due } = nextStatusSoundState(stateRef.current, sessions, now);
    stateRef.current = state;
    const events = new Set(
      due.filter((d) => !(d.event === "waiting" && approvalChimedRecently(d.id, now))).map((d) => d.event),
    );
    for (const event of events) void playSoundFor(event);
  }, [sessions]);

  // The server times the dwell, so a cue sounds without waiting for a session-list refresh.
  return useCallback(
    (id: string, event: StatusSoundEvent) => {
      const now = Date.now();
      const session = sessionsRef.current.find((x) => x.id === id);
      const { state, play } = soundDueFor(stateRef.current, session, event);
      stateRef.current = state;
      if (play && !(event === "waiting" && approvalChimedRecently(id, now))) void playSoundFor(event);
    },
    [sessionsRef],
  );
}
