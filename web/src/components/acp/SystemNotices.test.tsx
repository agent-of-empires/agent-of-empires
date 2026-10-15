// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { RateLimitRecoverySection, SystemNotices } from "./SystemNotices";

vi.mock("../../lib/api", () => ({
  fetchAcpAgents: vi.fn(),
  switchAcpAgent: vi.fn(),
  fetchContextPrimer: vi.fn(),
}));

import { fetchAcpAgents, switchAcpAgent, fetchContextPrimer } from "../../lib/api";

afterEach(() => {
  cleanup();
});

const LIMITED = { status: "limited", resets_at: "2099-01-01T00:00:00Z", kind: "rate_limit" };
const PARIS = "Internal error: You've hit your weekly limit · resets 4am (Europe/Paris)";

type NoticeProps = React.ComponentProps<typeof SystemNotices>;

function noticeProps(overrides?: Partial<NoticeProps>): NoticeProps {
  return {
    status: "open",
    lagged: false,
    rateLimit: null,
    rateLimitRetriesExhausted: false,
    hasEverOpened: true,
    reconnecting: false,
    retryCount: 0,
    retryCountdown: 0,
    maxRetries: 7,
    manualReconnect: vi.fn(),
    ...overrides,
  };
}

const mount = (overrides?: Partial<NoticeProps>) => render(<SystemNotices {...noticeProps(overrides)} />);

describe("SystemNotices", () => {
  it.each([
    [true, /Auto-resume is armed/],
    [false, /Auto-resume is off for this profile/],
    // Unknown: claim nothing.
    [undefined, null],
  ])("auto-resume %s", (rateLimitAutoResume, expected) => {
    const { queryByText } = mount({ rateLimit: LIMITED, rateLimitAutoResume });
    if (expected) expect(queryByText(expected)).not.toBeNull();
    else expect(queryByText(/Auto-resume/)).toBeNull();
  });

  // Without a parseable reset, the banner shows the agent's own wording, never a made-up clock (#3174).
  it.each([
    [
      "a reported reset",
      { status: PARIS, resets_at: "2099-01-01T09:30:00Z" },
      `Rate-limited (rate_limit); resets at ${new Date("2099-01-01T09:30:00Z").toLocaleTimeString()}.`,
    ],
    [
      "no reset",
      { status: PARIS, resets_at: null },
      "Rate-limited (rate_limit); You've hit your weekly limit · resets 4am (Europe/Paris)",
    ],
    [
      "an unparseable reset",
      { status: PARIS, resets_at: "not-a-timestamp" },
      "Rate-limited (rate_limit); You've hit your weekly limit · resets 4am (Europe/Paris)",
    ],
    [
      "transport prefixes and a JSON fingerprint",
      {
        status:
          'ACP connection failed: Internal error: You\'ve hit your limit · resets 12:10pm (Europe/Paris): {\n  "errorKind":"rate_limit"\n}',
        resets_at: null,
      },
      "Rate-limited (rate_limit); You've hit your limit · resets 12:10pm (Europe/Paris)",
    ],
    [
      "only a fingerprint",
      { status: '{"errorKind":"rate_limit"}', resets_at: null },
      "Rate-limited (rate_limit); the agent did not report a reset time.",
    ],
  ])("words the rate limit for %s", (_label, limit, expected) => {
    const { getByText, container } = mount({ rateLimit: { ...limit, kind: "rate_limit" } });
    expect(getByText(expected)).toBeDefined();
    expect(container.textContent).not.toMatch(/Invalid Date|errorKind|ACP connection failed/);
    if (!expected.includes("resets at")) expect(container.textContent).not.toMatch(/resets at \d/);
  });

  it("shows both recovery actions only with a rate limit and their handlers", () => {
    const onSwitchAgent = vi.fn();
    const onResumeRateLimit = vi.fn();
    const { getByRole, queryByRole, rerender } = mount({ rateLimit: LIMITED, onSwitchAgent, onResumeRateLimit });
    fireEvent.click(getByRole("button", { name: /continue in another agent/i }));
    fireEvent.click(getByRole("button", { name: /resume now/i }));
    expect(onSwitchAgent).toHaveBeenCalledTimes(1);
    expect(onResumeRateLimit).toHaveBeenCalledTimes(1);

    rerender(<SystemNotices {...noticeProps({ rateLimit: LIMITED })} />);
    expect(queryByRole("button", { name: /continue in another agent/i })).toBeNull();

    rerender(
      <SystemNotices
        {...noticeProps({
          reconnecting: true,
          status: "connecting",
          retryCount: 1,
          retryCountdown: 3,
          onSwitchAgent,
          onResumeRateLimit,
        })}
      />,
    );
    expect(queryByRole("button", { name: /continue in another agent/i })).toBeNull();
    expect(queryByRole("button", { name: /resume now/i })).toBeNull();
  });

  it.each([
    ["retrying", /resuming/i, null],
    ["ok", /resume requested/i, /Resume requested\. New events should start streaming shortly/i],
  ] as const)("disables Resume while %s", (rateLimitResumeState, name, note) => {
    const { getByRole, queryByText } = mount({ rateLimit: LIMITED, onResumeRateLimit: vi.fn(), rateLimitResumeState });
    expect((getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(true);
    if (note) expect(queryByText(note)).not.toBeNull();
  });

  it("shows failed resume feedback while retaining both actions", () => {
    const { getByRole, getByText } = mount({
      rateLimit: LIMITED,
      onResumeRateLimit: vi.fn(),
      onSwitchAgent: vi.fn(),
      rateLimitResumeState: "failed",
      rateLimitResumeError: "Server returned 500. spawn failed",
    });
    expect(getByText(/Resume failed: Server returned 500\. spawn failed/i)).toBeDefined();
    expect(getByRole("button", { name: /resume now/i })).toBeDefined();
    expect(getByRole("button", { name: /continue in another agent/i })).toBeDefined();
  });

  // `Stopped` does not clear `rate_limit` server-side, so a cap park keeps the snapshot and both buttons (#3693).
  it.each([
    [{ status: "limited", resets_at: "2099-01-01T00:00:00Z", kind: "usage" }, true],
    [null, false],
  ])("shows the auto-resume stopped note (snapshot %o)", (rateLimit, buttons) => {
    const { getByText, queryByRole } = mount({
      rateLimitRetriesExhausted: true,
      rateLimit,
      onSwitchAgent: vi.fn(),
      onResumeRateLimit: vi.fn(),
    });
    expect(getByText(/Auto-resume stopped: the same prompt was re-sent too many times/i)).toBeDefined();
    expect(queryByRole("button", { name: /resume now/i }) !== null).toBe(buttons);
    expect(queryByRole("button", { name: /continue in another agent/i }) !== null).toBe(buttons);
  });
});

describe("RateLimitRecoverySection", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(fetchAcpAgents).mockResolvedValue([
      { name: "claude", description: "Claude", command: "claude-agent-acp" },
      { name: "codex", description: "OpenAI Codex", command: "codex-acp" },
    ]);
    vi.mocked(switchAcpAgent).mockResolvedValue({
      session_id: "s-1",
      agent: "codex",
      before_seq: 5,
      switch_seq: 6,
      status: "switched",
    });
    vi.mocked(fetchContextPrimer).mockResolvedValue({
      primer: "ctx",
      included_event_count: 1,
      included_turn_count: 1,
      truncated: false,
      max_chars: 4_000,
      unprocessed_prompt: "deploy",
    });
  });

  it("opens the modal from the children trigger, forwards the handoff prefill, and closes", async () => {
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
    expect(queryByText(/Continue in another agent\?/i)).toBeNull();
    fireEvent.click(getByText("handoff"));
    await findByText(/Continue in another agent\?/i);
    fireEvent.click(await findByText(/Continue in codex/));
    await waitFor(() => expect(onPrefill).toHaveBeenCalledTimes(1));
    const prefilled = onPrefill.mock.calls[0]?.[0] as string;
    expect(prefilled).toContain("CONTEXT HANDOFF");
    expect(prefilled).toContain("deploy");
    await waitFor(() => expect(queryByText(/Continue in another agent\?/i)).toBeNull());
  });
});
