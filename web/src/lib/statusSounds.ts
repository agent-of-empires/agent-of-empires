import type { SessionResponse, SessionStatus } from "./types";

export type StatusSoundEvent = "waiting" | "idle" | "error";

// A status must hold this long before it sounds, so a flap is silent. Mirrors the push delays in src/server/push.rs.
const DWELL_MS: Record<StatusSoundEvent, number> = { waiting: 5000, idle: 2000, error: 2000 };

const EVENT_FOR_STATUS: Partial<Record<SessionStatus, StatusSoundEvent>> = {
  Waiting: "waiting",
  Idle: "idle",
  Error: "error",
};

interface Tracked {
  status: SessionStatus;
  since: number;
  sounded: boolean;
}

export type StatusSoundState = ReadonlyMap<string, Tracked>;

export interface DueSound {
  id: string;
  event: StatusSoundEvent;
}

function eventFor(s: SessionResponse): StatusSoundEvent | undefined {
  if (s.archived_at != null || s.snoozed_until != null || s.trashed_at != null) return undefined;
  return EVENT_FOR_STATUS[s.status];
}

/**
 * Folds one session-list snapshot into the tracker. `prev` is null for the first snapshot, which only sets the
 * baseline, and a session seen for the first time is baselined too, so nothing sounds for what was already there.
 */
export function nextStatusSoundState(
  prev: StatusSoundState | null,
  sessions: readonly SessionResponse[],
  now: number,
): { state: StatusSoundState; due: DueSound[] } {
  const state = new Map<string, Tracked>();
  const due: DueSound[] = [];
  for (const s of sessions) {
    const before = prev?.get(s.id);
    let tracked: Tracked;
    if (!before) {
      tracked = { status: s.status, since: now, sounded: true };
    } else if (before.status !== s.status) {
      tracked = { status: s.status, since: now, sounded: false };
    } else {
      tracked = { ...before };
    }
    if (!tracked.sounded) {
      const event = eventFor(s);
      if (!event) {
        tracked.sounded = true;
      } else if (now - tracked.since >= DWELL_MS[event]) {
        tracked.sounded = true;
        due.push({ id: s.id, event });
      }
    }
    state.set(s.id, tracked);
  }
  return { state, due };
}
