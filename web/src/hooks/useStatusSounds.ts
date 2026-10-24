import { useEffect, useRef } from "react";
import type { SessionResponse } from "../lib/types";
import { approvalChimedRecently, playStatusSounds } from "../lib/soundPlayer";
import { nextStatusSoundState, type StatusSoundState } from "../lib/statusSounds";

/** Plays the waiting, idle and error sounds for every session, from each session list refresh. */
export function useStatusSounds(sessions: readonly SessionResponse[], activeSessionId: string | null): void {
  const stateRef = useRef<StatusSoundState | null>(null);

  useEffect(() => {
    const now = Date.now();
    const { state, due } = nextStatusSoundState(stateRef.current, sessions, now);
    stateRef.current = state;
    void playStatusSounds(
      due.filter((d) => !(d.event === "waiting" && approvalChimedRecently(d.id, now))),
      activeSessionId,
    );
  }, [sessions, activeSessionId]);
}
