// @vitest-environment jsdom
//
// RateLimitRecoverySection is the small wrapper that owns the
// recovery-modal open/close state and feeds the trigger callback down
// to whatever banner StructuredView passes as `children`. Mounting all of
// StructuredView to exercise these few lines would require ~20 hook
// mocks; this test pins the wiring directly.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { RateLimitRecoverySection } from "./StructuredView";

vi.mock("../../lib/api", () => ({
  fetchAcpAgents: vi.fn(),
  switchAcpAgent: vi.fn(),
  fetchContextPrimer: vi.fn(),
}));

import { fetchAcpAgents, switchAcpAgent, fetchContextPrimer } from "../../lib/api";

const mockFetchAgents = vi.mocked(fetchAcpAgents);
const mockSwitch = vi.mocked(switchAcpAgent);
const mockPrimer = vi.mocked(fetchContextPrimer);

beforeEach(() => {
  vi.clearAllMocks();
  mockFetchAgents.mockResolvedValue([
    { name: "claude", description: "Claude", command: "claude-agent-acp" },
    { name: "codex", description: "OpenAI Codex", command: "codex-acp" },
  ]);
  mockSwitch.mockResolvedValue({
    session_id: "s-1",
    agent: "codex",
    before_seq: 5,
    switch_seq: 6,
    status: "switched",
  });
  mockPrimer.mockResolvedValue({
    primer: "ctx",
    included_event_count: 1,
    included_turn_count: 1,
    truncated: false,
    max_chars: 4_000,
    unprocessed_prompt: "deploy",
  });
});

afterEach(() => {
  cleanup();
});

describe("RateLimitRecoverySection", () => {
  it("modal stays closed by default and opens on the children trigger", async () => {
    const onPrefill = vi.fn();
    const { findByText, getByText, queryByText } = render(
      <RateLimitRecoverySection sessionId="s-1" currentAgent="claude" onPrefill={onPrefill}>
        {({ onSwitchAgent }) => (
          <button type="button" onClick={onSwitchAgent}>
            handoff
          </button>
        )}
      </RateLimitRecoverySection>,
    );
    // Modal not visible yet.
    expect(queryByText(/Continue in another agent\?/i)).toBeNull();
    fireEvent.click(getByText("handoff"));
    // Modal renders its header now.
    await findByText(/Continue in another agent\?/i);
  });

  it("forwards the handoff prefill text to onPrefill and closes", async () => {
    const onPrefill = vi.fn();
    const { findByText, getByText, queryByText } = render(
      <RateLimitRecoverySection sessionId="s-1" currentAgent="claude" onPrefill={onPrefill}>
        {({ onSwitchAgent }) => (
          <button type="button" onClick={onSwitchAgent}>
            handoff
          </button>
        )}
      </RateLimitRecoverySection>,
    );
    fireEvent.click(getByText("handoff"));
    const confirm = await findByText(/Continue in codex/);
    fireEvent.click(confirm);
    await waitFor(() => expect(onPrefill).toHaveBeenCalledTimes(1));
    const prefilled = onPrefill.mock.calls[0]?.[0] as string;
    expect(prefilled).toContain("CONTEXT HANDOFF");
    expect(prefilled).toContain("deploy");
    // Modal closes after a successful switch.
    await waitFor(() => expect(queryByText(/Continue in another agent\?/i)).toBeNull());
  });

  it("renders children with the current onSwitchAgent prop signature", () => {
    const onPrefill = vi.fn();
    const childrenFn = vi.fn(({ onSwitchAgent }) => {
      // Assert at render time that the callback exists and is callable.
      expect(typeof onSwitchAgent).toBe("function");
      return null;
    });
    render(
      <RateLimitRecoverySection sessionId="s-1" currentAgent="claude" onPrefill={onPrefill}>
        {childrenFn}
      </RateLimitRecoverySection>,
    );
    expect(childrenFn).toHaveBeenCalled();
  });
});
