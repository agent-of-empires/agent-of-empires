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
});
