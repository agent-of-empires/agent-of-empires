import { describe, expect, it } from "vitest";
import type { SessionStatus } from "./types";
import { nextStatusSoundState, soundDueFor, type StatusSoundState } from "./statusSounds";
import { session } from "../hooks/__tests__/fixtures";

const at = (id: string, status: SessionStatus, over = {}) => session({ id, status, ...over });

/** Feeds `[time, sessions]` snapshots through the tracker and returns each snapshot's due list. */
function run(...snapshots: [number, ReturnType<typeof at>[]][]) {
  let state: StatusSoundState | null = null;
  return snapshots.map(([now, sessions]) => {
    const next = nextStatusSoundState(state, sessions, now);
    state = next.state;
    return next.due;
  });
}

describe("nextStatusSoundState", () => {
  it("is silent for statuses present in the first snapshot", () => {
    const [first, later] = run(
      [0, [at("a", "Waiting"), at("b", "Idle"), at("c", "Error")]],
      [10_000, [at("a", "Waiting"), at("b", "Idle"), at("c", "Error")]],
    );
    expect(first).toEqual([]);
    expect(later).toEqual([]);
  });

  it("is silent for a session seen for the first time later", () => {
    const [, , due] = run(
      [0, [at("a", "Running")]],
      [3000, [at("a", "Running"), at("b", "Idle")]],
      [9000, [at("a", "Running"), at("b", "Idle")]],
    );
    expect(due).toEqual([]);
  });

  it.each([
    ["Idle", "idle", 2000],
    ["Error", "error", 2000],
    ["Waiting", "waiting", 5000],
  ] as const)("sounds %s once, after its dwell", (status, event, dwell) => {
    const [, early, due, again] = run(
      [0, [at("a", "Running")]],
      [1000, [at("a", status)]],
      [1000 + dwell, [at("a", status)]],
      [1000 + dwell + 3000, [at("a", status)]],
    );
    expect(early).toEqual([]);
    expect(due).toEqual([{ id: "a", event }]);
    expect(again).toEqual([]);
  });

  it("is silent when the status flips back before the dwell", () => {
    const due = run(
      [0, [at("a", "Running")]],
      [1000, [at("a", "Waiting")]],
      [4000, [at("a", "Running")]],
      [9000, [at("a", "Running")]],
    );
    expect(due.flat()).toEqual([]);
  });

  it("sounds again for a later transition", () => {
    const due = run(
      [0, [at("a", "Running")]],
      [1000, [at("a", "Idle")]],
      [4000, [at("a", "Idle")]],
      [5000, [at("a", "Running")]],
      [8000, [at("a", "Idle")]],
      [11_000, [at("a", "Idle")]],
    );
    expect(due.flat()).toEqual([
      { id: "a", event: "idle" },
      { id: "a", event: "idle" },
    ]);
  });

  it.each(["Running", "Starting", "Stopped", "Unknown", "Creating", "Deleting"] as const)(
    "never sounds for %s",
    (status) => {
      const due = run([0, [at("a", "Idle")]], [1000, [at("a", status)]], [20_000, [at("a", status)]]);
      expect(due.flat()).toEqual([]);
    },
  );

  it.each([
    ["archived", { archived_at: "2025-01-01T00:00:00Z" }],
    ["snoozed", { snoozed_until: "2099-01-01T00:00:00Z" }],
    ["trashed", { trashed_at: "2025-01-01T00:00:00Z" }],
  ])("is silent for %s sessions", (_label, over) => {
    const due = run([0, [at("a", "Running", over)]], [1000, [at("a", "Idle", over)]], [9000, [at("a", "Idle", over)]]);
    expect(due.flat()).toEqual([]);
  });

  it.each([
    ["idle", "notify_on_idle"],
    ["waiting", "notify_on_waiting"],
    ["error", "notify_on_error"],
  ] as const)("is silent for a session muted for %s", (event, flag) => {
    const status = { idle: "Idle", waiting: "Waiting", error: "Error" }[event] as SessionStatus;
    for (const [value, expected] of [
      [false, []],
      [null, [{ id: "a", event }]],
      [true, [{ id: "a", event }]],
    ] as const) {
      const due = run(
        [0, [at("a", "Running", { [flag]: value })]],
        [1000, [at("a", status, { [flag]: value })]],
        [9000, [at("a", status, { [flag]: value })]],
      );
      expect(due.flat(), `${flag}=${value}`).toEqual(expected);
    }
  });

  it("reports each due session separately", () => {
    const due = run(
      [0, [at("a", "Running"), at("b", "Running")]],
      [1000, [at("a", "Idle"), at("b", "Error")]],
      [4000, [at("a", "Idle"), at("b", "Error")]],
    );
    expect(due[2]).toEqual([
      { id: "a", event: "idle" },
      { id: "b", event: "error" },
    ]);
  });
});

describe("soundDueFor", () => {
  const tracked = (sounded: boolean): StatusSoundState =>
    new Map([["a", { status: "Waiting" as SessionStatus, since: 0, sounded }]]);

  it.each([
    ["sounds an unsounded status", at("a", "Waiting"), "waiting", tracked(false), true],
    ["sounds a status the tracker has not seen", at("a", "Waiting"), "waiting", null, true],
    ["is silent once already sounded", at("a", "Waiting"), "waiting", tracked(true), false],
    ["is silent when the status moved on", at("a", "Running"), "waiting", tracked(false), false],
    [
      "is silent for a muted session",
      at("a", "Waiting", { notify_on_waiting: false }),
      "waiting",
      tracked(false),
      false,
    ],
    ["is silent for an unknown session", undefined, "waiting", tracked(false), false],
  ] as const)("%s", (_label, session, event, prev, play) => {
    const result = soundDueFor(prev, session, event, 100);
    expect(result.play).toBe(play);
    if (play) expect(result.state?.get("a")?.sounded).toBe(true);
  });

  it("keeps the later poll from sounding the same status again", () => {
    const cue = soundDueFor(null, at("a", "Waiting"), "waiting", 1000);
    const next = nextStatusSoundState(cue.state, [at("a", "Waiting")], 20_000);
    expect(next.due).toEqual([]);
  });
});
