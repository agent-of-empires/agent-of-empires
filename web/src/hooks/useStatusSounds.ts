import { useEffect, useRef } from "react";
import type { SessionResponse } from "../lib/types";
import { approvalChimedRecently, playSoundFor } from "../lib/soundPlayer";
import { nextStatusSoundState, type StatusSoundState } from "../lib/statusSounds";

/** Plays the waiting, idle and error sounds for every session, from each session list refresh. */
export function useStatusSounds(sessions: readonly SessionResponse[]): void {
  const stateRef = useRef<StatusSoundState | null>(null);

  useEffect(() => {
    const now = Date.now();
    const { state, due } = nextStatusSoundState(stateRef.current, sessions, now);
    stateRef.current = state;
    const events = new Set(
      due.filter((d) => !(d.event === "waiting" && approvalChimedRecently(d.id, now))).map((d) => d.event),
    );
    for (const event of events) void playSoundFor(event);
  }, [sessions]);
}
