// @vitest-environment jsdom
//
// Wiring contract for the rate-limit recovery handoff button. StructuredView
// passes `onSwitchAgent={() => setRecoveryOpen(true)}` and the only way
// the user reaches the SwitchAgentModal from here is by clicking the button
// SystemNotices conditionally renders below the rate-limit banner. This
// test pins:
//   1. The button shows up exactly when both `rateLimit` and
//      `onSwitchAgent` are present (the modal-trigger path).
//   2. Clicking it invokes the handler with no args.
//   3. The button stays hidden when rateLimit is null or onSwitchAgent
//      is undefined, so a plain reconnect banner does not gain a
//      dangling "Continue in another agent" affordance.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { SystemNotices } from "./StructuredView";

afterEach(() => {
  cleanup();
});

function mount(
  overrides?: Partial<React.ComponentProps<typeof SystemNotices>>,
) {
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
    const { getByRole, queryByRole, rerender } = mount({
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      onSwitchAgent,
    });
    const button = getByRole("button", { name: /continue in another agent/i });
    expect(button).toBeDefined();

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
    expect(
      queryByRole("button", { name: /continue in another agent/i }),
    ).toBeNull();
  });

  it("hides the switch-agent button when rateLimit is null", () => {
    const { queryByRole } = mount({
      reconnecting: true,
      status: "connecting",
      retryCount: 1,
      retryCountdown: 3,
      onSwitchAgent: vi.fn(),
    });
    expect(
      queryByRole("button", { name: /continue in another agent/i }),
    ).toBeNull();
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
    fireEvent.click(
      getByRole("button", { name: /continue in another agent/i }),
    );
    expect(onSwitchAgent).toHaveBeenCalledTimes(1);
  });

  it("renders nothing for a healthy session", () => {
    const { container } = mount();
    expect(container.firstChild).toBeNull();
  });
});
