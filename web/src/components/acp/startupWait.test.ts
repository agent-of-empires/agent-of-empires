import { describe, expect, it } from "vitest";

import { emptyAcpState } from "../../lib/acpTypes";
import { isNewSessionStarting } from "./startupWait";

describe("isNewSessionStarting", () => {
  const state = (over: Partial<ReturnType<typeof emptyAcpState>> = {}) => ({ ...emptyAcpState(), ...over });
  it.each<[string, Parameters<typeof isNewSessionStarting>, boolean]>([
    ["the Worker is starting a new Session", [state(), "open", true, "resuming"], true],
    ["the first connection is opening", [state(), "connecting", false, "absent"], true],
    ["the first connection failed and retries", [state(), "error", false, "absent"], true],
    ["a resumed Session (it has events)", [state({ lastSeq: 7 }), "open", true, "resuming"], false],
    ["a reconnect after the first open", [state(), "connecting", true, "running"], false],
    ["the Worker is running", [state(), "open", true, "running"], false],
    ["a startup error shows its own banner", [state({ startupError: "boom" }), "open", true, "resuming"], false],
    ["a stopped Worker", [state({ workerStopped: true }), "open", true, "resuming"], false],
  ])("%s", (_label, args, expected) => {
    expect(isNewSessionStarting(...args)).toBe(expected);
  });
});
