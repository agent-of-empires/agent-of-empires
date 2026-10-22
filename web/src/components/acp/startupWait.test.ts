import { describe, expect, it } from "vitest";

import { emptyAcpState } from "../../lib/acpTypes";
import { isNewSessionStarting } from "./startupWait";

type Connection = Parameters<typeof isNewSessionStarting>[1];
const connection = (over: Partial<Connection> = {}): Connection => ({
  status: "open",
  hasEverOpened: true,
  reconnecting: false,
  retryCount: 0,
  maxRetries: 7,
  ...over,
});

describe("isNewSessionStarting", () => {
  const state = (over: Partial<ReturnType<typeof emptyAcpState>> = {}) => ({ ...emptyAcpState(), ...over });
  it.each<[string, Parameters<typeof isNewSessionStarting>, boolean]>([
    ["the Worker is starting a new Session", [state(), connection(), "resuming", null], true],
    [
      "the first connection is opening",
      [state(), connection({ status: "connecting", hasEverOpened: false }), "absent", null],
      true,
    ],
    [
      "the first connection failed and retries",
      [state(), connection({ status: "error", hasEverOpened: false }), "absent", null],
      true,
    ],
    [
      "first-dial backoff",
      [
        state(),
        connection({ status: "closed", hasEverOpened: false, reconnecting: true, retryCount: 1 }),
        "absent",
        null,
      ],
      true,
    ],
    [
      "first-dial retries exhausted",
      [state(), connection({ status: "closed", hasEverOpened: false, retryCount: 7 }), "absent", null],
      false,
    ],
    [
      "exhausted while Worker resuming",
      [state(), connection({ status: "error", hasEverOpened: false, retryCount: 7 }), "resuming", null],
      false,
    ],
    [
      "last retry still in progress",
      [
        state(),
        connection({ status: "connecting", hasEverOpened: false, reconnecting: true, retryCount: 7 }),
        "absent",
        null,
      ],
      true,
    ],
    ["a resumed Session (it has events)", [state({ lastSeq: 7 }), connection(), "resuming", null], false],
    ["a reconnect after the first open", [state(), connection({ status: "connecting" }), "running", null], false],
    ["the Worker is running", [state(), connection(), "running", null], false],
    ["a startup error shows its own banner", [state({ startupError: "boom" }), connection(), "resuming", null], false],
    [
      "an older Session whose events haven't loaded yet",
      [state(), connection({ status: "connecting", hasEverOpened: false }), "resuming", "acp-session-1"],
      false,
    ],
    ["a stopped Worker", [state({ workerStopped: true }), connection(), "resuming", null], false],
  ])("%s", (_label, args, expected) => {
    expect(isNewSessionStarting(...args)).toBe(expected);
  });
});
