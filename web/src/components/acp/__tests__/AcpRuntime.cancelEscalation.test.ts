import { describe, expect, it } from "vitest";

import { nextCancelAction } from "../AcpRuntime";

// Regression guard for #2237: the Stop button must be able to escalate to
// a force-end without the server-confirmed `cancelling` flag, because that
// flag never flips when the daemon has no in-flight prompt (an adopted or
// orphaned turn). The local "already requested" intent is what keeps the
// escape hatch reachable.
describe("nextCancelAction (#2237)", () => {
  it("first press with a clean state sends a graceful cancel", () => {
    expect(nextCancelAction(false, false)).toBe("cancel");
  });

  it("escalates to force once the user has already pressed Stop this turn", () => {
    // This is the case the old `if (acp.state.cancelling)` gate missed:
    // no server confirmation, but the user clicked Stop a second time.
    expect(nextCancelAction(false, true)).toBe("force");
  });

  it("escalates to force when the server confirmed the cancel", () => {
    expect(nextCancelAction(true, false)).toBe("force");
  });

  it("escalates to force when both signals are set", () => {
    expect(nextCancelAction(true, true)).toBe("force");
  });
});
