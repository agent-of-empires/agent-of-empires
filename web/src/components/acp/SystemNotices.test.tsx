// @vitest-environment jsdom
//
// Wiring contract for the rate-limit recovery buttons. StructuredView
// passes `onSwitchAgent={() => setRecoveryOpen(true)}` and the only way
// the user reaches the SwitchAgentModal from here is by clicking the handoff
// button SystemNotices conditionally renders below the rate-limit banner. The
// same banner now also exposes the same-agent Resume now callback.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { SystemNotices } from "./StructuredView";

afterEach(() => {
  cleanup();
});

function mount(overrides?: Partial<React.ComponentProps<typeof SystemNotices>>) {
  const manualReconnect = vi.fn();
  const props: React.ComponentProps<typeof SystemNotices> = {
    status: "open",
    lagged: false,
    rateLimit: null,
    rateLimitRetriesExhausted: false,
    hasEverOpened: true,
    reconnecting: false,
    retryCount: 0,
    retryCountdown: 0,
    maxRetries: 7,
    manualReconnect,
    ...overrides,
  };
  return { manualReconnect, ...render(<SystemNotices {...props} />) };
}

describe("SystemNotices rate-limit handoff", () => {
  it("renders the switch-agent button only when rateLimit + handler are set", () => {
    const onSwitchAgent = vi.fn();
    const onResumeRateLimit = vi.fn();
    const { getByRole, queryByRole, rerender } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onSwitchAgent,
      onResumeRateLimit,
    });
    const button = getByRole("button", { name: /continue in another agent/i });
    expect(button).toBeDefined();
    expect(getByRole("button", { name: /resume now/i })).toBeDefined();

    // Re-render with onSwitchAgent unset; button should disappear.
    rerender(
      <SystemNotices
        status="open"
        lagged={false}
        rateLimitRetriesExhausted={false}
        rateLimit={{
          status: "limited",
          resets_at: "2099-01-01T00:00:00Z",
          kind: "rate_limit",
        }}
        hasEverOpened
        reconnecting={false}
        retryCount={0}
        retryCountdown={0}
        maxRetries={7}
        manualReconnect={vi.fn()}
      />,
    );
    expect(queryByRole("button", { name: /continue in another agent/i })).toBeNull();
  });

  // #3152: with a reported reset the banner shows the clock. Without one it
  // must show what the agent said instead of a fabricated time.
  it("renders the reset clock only when the agent reported one", () => {
    const { getByText, queryByText, rerender } = mount({
      rateLimit: {
        status: "Internal error: You've hit your weekly limit · resets 4am (Europe/Paris)",
        resets_at: "2099-01-01T09:30:00Z",
        kind: "rate_limit",
      },
    });
    const expected = new Date("2099-01-01T09:30:00Z").toLocaleTimeString();
    expect(getByText(`Rate-limited (rate_limit); resets at ${expected}.`)).toBeDefined();

    rerender(
      <SystemNotices
        status="open"
        lagged={false}
        rateLimitRetriesExhausted={false}
        rateLimit={{
          status: "Internal error: You've hit your weekly limit · resets 4am (Europe/Paris)",
          resets_at: null,
          kind: "rate_limit",
        }}
        hasEverOpened
        reconnecting={false}
        retryCount={0}
        retryCountdown={0}
        maxRetries={7}
        manualReconnect={vi.fn()}
      />,
    );
    expect(
      getByText("Rate-limited (rate_limit); You've hit your weekly limit · resets 4am (Europe/Paris)"),
    ).toBeDefined();
    expect(queryByText(/resets at \d/)).toBeNull();
  });

  // An unparseable reset is the same story as none at all: show what the
  // agent said, never "Invalid Date". See #3152.
  it("falls back to the agent's wording when the reported reset is unparseable", () => {
    const { getByText, queryByText } = mount({
      rateLimit: {
        status: "Internal error: You've hit your weekly limit · resets 4am (Europe/Paris)",
        resets_at: "not-a-timestamp",
        kind: "rate_limit",
      },
    });
    expect(
      getByText("Rate-limited (rate_limit); You've hit your weekly limit · resets 4am (Europe/Paris)"),
    ).toBeDefined();
    expect(queryByText(/Invalid Date/)).toBeNull();
  });

  // The connection-end path (`classify_rate_limit_from_message`) puts the whole
  // error Display string in `status`, transport prefix and the raw
  // `{"errorKind":"rate_limit"}` fingerprint included, and that path never has
  // a reported reset. The banner must not render the JSON payload. See #3152.
  it("strips transport prefixes and the JSON fingerprint from the agent's wording", () => {
    const { getByText, queryByText } = mount({
      rateLimit: {
        status:
          'ACP connection failed: Internal error: You\'ve hit your limit · resets 12:10pm (Europe/Paris): {\n  "errorKind":"rate_limit"\n}',
        resets_at: null,
        kind: "rate_limit",
      },
    });
    expect(getByText("Rate-limited (rate_limit); You've hit your limit · resets 12:10pm (Europe/Paris)")).toBeDefined();
    expect(queryByText(/errorKind/)).toBeNull();
    expect(queryByText(/ACP connection failed/)).toBeNull();
  });

  // Nothing but the fingerprint: there is no wording to show, so say so rather
  // than leaving a dangling "Rate-limited (rate_limit); ".
  it("falls back to a sentence when the status carries no wording at all", () => {
    const { getByText } = mount({
      rateLimit: { status: '{"errorKind":"rate_limit"}', resets_at: null, kind: "rate_limit" },
    });
    expect(getByText("Rate-limited (rate_limit); the agent did not report a reset time.")).toBeDefined();
  });

  it("hides the switch-agent button when rateLimit is null", () => {
    const { queryByRole } = mount({
      reconnecting: true,
      status: "connecting",
      retryCount: 1,
      retryCountdown: 3,
      onSwitchAgent: vi.fn(),
      onResumeRateLimit: vi.fn(),
    });
    expect(queryByRole("button", { name: /continue in another agent/i })).toBeNull();
    expect(queryByRole("button", { name: /resume now/i })).toBeNull();
  });

  it("invokes onSwitchAgent on click", () => {
    const onSwitchAgent = vi.fn();
    const { getByRole } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onSwitchAgent,
    });
    fireEvent.click(getByRole("button", { name: /continue in another agent/i }));
    expect(onSwitchAgent).toHaveBeenCalledTimes(1);
  });

  it("invokes onResumeRateLimit on click", () => {
    const onResumeRateLimit = vi.fn();
    const { getByRole } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onResumeRateLimit,
    });
    fireEvent.click(getByRole("button", { name: /resume now/i }));
    expect(onResumeRateLimit).toHaveBeenCalledTimes(1);
  });

  it("disables Resume now while retrying", () => {
    const { getByRole } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onResumeRateLimit: vi.fn(),
      rateLimitResumeState: "retrying",
    });
    const button = getByRole("button", { name: /resuming/i }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });

  it("keeps Resume now disabled after a successful resume request", () => {
    const { getByRole, getByText } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onResumeRateLimit: vi.fn(),
      rateLimitResumeState: "ok",
    });
    const button = getByRole("button", { name: /resume requested/i }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(getByText(/Resume requested\. New events should start streaming shortly/i)).toBeDefined();
  });

  it("shows failed resume feedback while retaining both actions", () => {
    const { getByRole, getByText } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onResumeRateLimit: vi.fn(),
      onSwitchAgent: vi.fn(),
      rateLimitResumeState: "failed",
      rateLimitResumeError: "Server returned 500. spawn failed",
    });
    expect(getByText(/Resume failed: Server returned 500\. spawn failed/i)).toBeDefined();
    expect(getByRole("button", { name: /resume now/i })).toBeDefined();
    expect(getByRole("button", { name: /continue in another agent/i })).toBeDefined();
  });

  it("renders nothing for a healthy session", () => {
    const { container } = mount();
    expect(container.firstChild).toBeNull();
  });

  // #3688: the state a real cap park reaches. `Stopped` does not clear
  // `rate_limit` in the server fold, so the adapter snapshot from the last
  // rejection is still there and both recovery buttons render alongside the
  // give-up note. Mounting without the snapshot would assert a combination
  // the daemon never produces.
  it("shows the auto-resume stopped note with both recovery paths still offered", () => {
    const onSwitchAgent = vi.fn();
    const onResumeRateLimit = vi.fn();
    const { getByText, getByRole } = mount({
      rateLimitRetriesExhausted: true,
      rateLimit: { status: "limited", resets_at: "2099-01-01T00:00:00Z", kind: "usage" },
      onSwitchAgent,
      onResumeRateLimit,
    });
    expect(getByText(/Auto-resume stopped: the same prompt was re-sent too many times/i)).toBeDefined();
    expect(getByRole("button", { name: /resume now/i })).toBeDefined();
    expect(getByRole("button", { name: /continue in another agent/i })).toBeDefined();
  });

  // The park outlives the snapshot only after a resume clears it, and the
  // note must survive that on its own so the banner does not vanish.
  it("shows the note with no rate-limit snapshot, without recovery buttons", () => {
    const { getByText, queryByRole } = mount({
      rateLimitRetriesExhausted: true,
      onResumeRateLimit: vi.fn(),
    });
    expect(getByText(/Auto-resume stopped: the same prompt was re-sent too many times/i)).toBeDefined();
    expect(queryByRole("button", { name: /resume now/i })).toBeNull();
  });
});
