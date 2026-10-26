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
    ["the Worker is starting a new Session", [state(), connection(), "resuming"], true],
    [
      "the first connection is opening",
      [state(), connection({ status: "connecting", hasEverOpened: false }), "absent"],
      true,
    ],
    [
      "the first connection failed and retries",
      [state(), connection({ status: "error", hasEverOpened: false }), "absent"],
      true,
    ],
    [
      "first-dial backoff",
      [state(), connection({ status: "closed", hasEverOpened: false, reconnecting: true, retryCount: 1 }), "absent"],
      true,
    ],
    [
      "first-dial retries exhausted",
      [state(), connection({ status: "closed", hasEverOpened: false, retryCount: 7 }), "absent"],
      false,
    ],
    [
      "exhausted while Worker resuming",
      [state(), connection({ status: "error", hasEverOpened: false, retryCount: 7 }), "resuming"],
      false,
    ],
    [
      "last retry still in progress",
      [
        state(),
        connection({ status: "connecting", hasEverOpened: false, reconnecting: true, retryCount: 7 }),
        "absent",
      ],
      true,
    ],
    ["a resumed Session (it has events)", [state({ lastSeq: 7 }), connection(), "resuming"], false],
    ["a reconnect after the first open", [state(), connection({ status: "connecting" }), "running"], false],
    ["the Worker is running", [state(), connection(), "running"], false],
    ["a startup error shows its own banner", [state({ startupError: "boom" }), connection(), "resuming"], false],
    ["a stopped Worker", [state({ workerStopped: true }), connection(), "resuming"], false],
  ])("%s", (_label, args, expected) => {
    expect(isNewSessionStarting(...args)).toBe(expected);
  });
});
