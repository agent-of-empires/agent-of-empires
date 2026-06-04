// @vitest-environment jsdom
//
// Unit tests for the cross-component switch-agent trigger (#1747): the
// dispatch + pending-latch helper the sidebar context menu uses to ask a
// (possibly not-yet-mounted) structured view Composer to open its switch-agent
// dialog. Asserts both the dispatched event and the stashed latch.

import { afterEach, describe, expect, it } from "vitest";
import {
  OPEN_SWITCH_AGENT_EVENT,
  consumePendingSwitchAgent,
  requestSwitchAgent,
  type OpenSwitchAgentDetail,
} from "../switchAgentTrigger";

function captureDispatch(): () => OpenSwitchAgentDetail[] {
  const seen: OpenSwitchAgentDetail[] = [];
  const handler = (e: Event) => {
    seen.push((e as CustomEvent<OpenSwitchAgentDetail>).detail);
  };
  window.addEventListener(OPEN_SWITCH_AGENT_EVENT, handler);
  return () => {
    window.removeEventListener(OPEN_SWITCH_AGENT_EVENT, handler);
    return seen;
  };
}

afterEach(() => {
  // Drain any latch left over so tests stay independent.
  consumePendingSwitchAgent("s1");
  consumePendingSwitchAgent("s2");
});

describe("requestSwitchAgent", () => {
  it("dispatches an event carrying the target session id", () => {
    const done = captureDispatch();
    requestSwitchAgent("s1");
    const seen = done();
    expect(seen).toEqual([{ sessionId: "s1" }]);
  });

  it("stashes the request so a later consume for the same id wins", () => {
    requestSwitchAgent("s1");
    expect(consumePendingSwitchAgent("s1")).toBe(true);
    // Latch is one-shot: a second consume returns false.
    expect(consumePendingSwitchAgent("s1")).toBe(false);
  });

  it("does not satisfy a consume for a different session", () => {
    requestSwitchAgent("s1");
    expect(consumePendingSwitchAgent("s2")).toBe(false);
    // The original latch is still pending for s1.
    expect(consumePendingSwitchAgent("s1")).toBe(true);
  });

  it("keeps only the most recent request when called twice", () => {
    requestSwitchAgent("s1");
    requestSwitchAgent("s2");
    expect(consumePendingSwitchAgent("s1")).toBe(false);
    expect(consumePendingSwitchAgent("s2")).toBe(true);
  });
});
