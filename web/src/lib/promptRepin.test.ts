import { describe, expect, it } from "vitest";

import { promptRepinDecision } from "./promptRepin";

/** Feed a sequence of (promptSeq, live, localInflight) observations from a
 *  fresh mount and return which steps asked for a pin. */
function run(steps: Array<[number, boolean, boolean?]>): boolean[] {
  let seen: number | null = null;
  return steps.map(([promptSeq, live, localInflight = false]) => {
    const d = promptRepinDecision({ seen, promptSeq, live, localInflight });
    seen = d.seen;
    return d.pin;
  });
}

describe("promptRepinDecision (#3993)", () => {
  it("pins once per prompt dispatched after mount, never on the mount pass", () => {
    expect(
      run([
        [3, true],
        [4, true],
        [4, true],
        [5, true],
      ]),
    ).toEqual([false, true, false, true]);
  });

  it("ignores hydration bumps: prompts replayed before the socket opens are not submits", () => {
    // Cold open: promptSeq climbs as the replay tail lands, then onopen flips
    // live, then the first real prompt of this session is sent.
    expect(
      run([
        [0, false],
        [12, false],
        [12, true],
        [13, true],
      ]),
    ).toEqual([false, false, false, true]);
  });

  it("never pins on a reset or a decrease, and counts the next prompt again", () => {
    expect(
      run([
        [7, true],
        [0, true],
        [1, true],
      ]),
    ).toEqual([false, false, true]);
  });

  it("pins a prompt this client sent before the socket first opened", () => {
    // sendPrompt dispatches the optimistic row (and bumps promptSeq) before the
    // POST, even before onopen; the in-flight id tells it apart from a replay.
    expect(
      run([
        [12, false],
        [13, false, true],
        [13, true, false],
      ]),
    ).toEqual([false, true, false]);
  });

  it("carries the observed counter forward regardless of the pin verdict", () => {
    const base = { localInflight: false };
    expect(promptRepinDecision({ ...base, seen: null, promptSeq: 9, live: true })).toEqual({ seen: 9, pin: false });
    expect(promptRepinDecision({ ...base, seen: 9, promptSeq: 10, live: false })).toEqual({ seen: 10, pin: false });
    expect(promptRepinDecision({ ...base, seen: 10, promptSeq: 11, live: true })).toEqual({ seen: 11, pin: true });
  });
});
