import { describe, expect, it } from "vitest";

import { applyEvent, emptyAcpState, type AcpEvent } from "../../lib/acpTypes";
import { reducer } from "../../hooks/acpSession/reducer";
import { hasStartedBefore, isNewSessionStarting } from "./startupWait";

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
  const replay = (...events: AcpEvent[]) =>
    events.reduce((s, event, i) => applyEvent(s, { session_id: "s", seq: i + 1, event }), emptyAcpState());
  // What `initialize` persists before `session/new` answers.
  const beforeSessionNew: AcpEvent[] = [
    { PromptCapabilities: { image: false, audio: false, embedded_context: false } },
    { AvailableCommandsUpdated: { commands: [] } },
  ];
  const assigned = replay(...beforeSessionNew, { AcpSessionAssigned: { acp_session_id: "acp-1" } });
  const restarted = reducer(replay(...beforeSessionNew, { Stopped: { reason: "user_stopped" } }), {
    kind: "restart_started",
    epoch: 0,
    stoppedSent: false,
  });
  it.each<[string, Parameters<typeof isNewSessionStarting>, boolean]>([
    ["the Worker is starting a new Session", [state(), connection(), "resuming", false], true],
    ["the startup line restarted the start", [restarted, connection(), "resuming", false], true],
    [
      "the first connection is opening",
      [state(), connection({ status: "connecting", hasEverOpened: false }), "absent", false],
      true,
    ],
    [
      "the first connection failed and retries",
      [state(), connection({ status: "error", hasEverOpened: false }), "absent", false],
      true,
    ],
    [
      "first-dial backoff",
      [
        state(),
        connection({ status: "closed", hasEverOpened: false, reconnecting: true, retryCount: 1 }),
        "absent",
        false,
      ],
      true,
    ],
    [
      "first-dial retries exhausted",
      [state(), connection({ status: "closed", hasEverOpened: false, retryCount: 7 }), "absent", false],
      false,
    ],
    [
      "exhausted while Worker resuming",
      [state(), connection({ status: "error", hasEverOpened: false, retryCount: 7 }), "resuming", false],
      false,
    ],
    [
      "last retry still in progress",
      [
        state(),
        connection({ status: "connecting", hasEverOpened: false, reconnecting: true, retryCount: 7 }),
        "absent",
        false,
      ],
      true,
    ],
    ["a resumed Session (its session was assigned)", [assigned, connection(), "resuming", false], false],
    ["a reconnect after the first open", [assigned, connection({ status: "connecting" }), "running", false], false],
    ["the Worker is running with its session assigned", [assigned, connection(), "running", false], false],
    [
      "a running Worker whose session/new hasn't answered, past initialize's events",
      [replay(...beforeSessionNew), connection(), "running", false],
      true,
    ],
    [
      "a reconnect while session/new hasn't answered",
      [replay(...beforeSessionNew), connection({ status: "connecting" }), "running", false],
      true,
    ],
    ["a startup error shows its own banner", [state({ startupError: "boom" }), connection(), "resuming", false], false],
    [
      "an older Session whose events haven't loaded yet",
      [state(), connection({ status: "connecting", hasEverOpened: false }), "resuming", true],
      false,
    ],
    ["a stopped Worker", [state({ workerStopped: true }), connection(), "resuming", false], false],
    [
      "a running Worker while the first connection opens",
      [state(), connection({ status: "connecting", hasEverOpened: false }), "running", false],
      false,
    ],
    [
      "a running Worker while the first connection fails and retries",
      [state(), connection({ status: "error", hasEverOpened: false }), "running", false],
      false,
    ],
    [
      "a running Worker during first-dial backoff",
      [
        state(),
        connection({ status: "closed", hasEverOpened: false, reconnecting: true, retryCount: 1 }),
        "running",
        false,
      ],
      false,
    ],
  ])("%s", (_label, args, expected) => {
    expect(isNewSessionStarting(...args)).toBe(expected);
  });
  it("keeps a prompt accepted during a stalled session/new pending, so Restart warns about it", () => {
    const stalled = replay(...beforeSessionNew, { UserPromptSent: { text: "hi", prompt_id: "p1" } });
    expect(stalled.turnActive).toBe(true);
    expect(isNewSessionStarting(stalled, connection(), "running", false)).toBe(true);
  });
});

describe("hasStartedBefore", () => {
  it.each([
    ["a new Session", {}, false],
    ["an older Session", { acp_session_id: "acp-1" }, true],
    ["an import whose history hasn't loaded", { acp_session_id: "acp-1", import_pending: true }, false],
  ])("%s", (_label, session, expected) => {
    expect(hasStartedBefore(session)).toBe(expected);
  });
});
