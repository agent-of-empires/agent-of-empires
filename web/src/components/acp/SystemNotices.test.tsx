// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";

import { SystemNotices } from "./SystemNotices";
import { emptyAcpState } from "../../lib/acpTypes";
import { isNewSessionStarting } from "./startupWait";
import { setServerDown } from "../../lib/connectionState";

afterEach(() => {
  cleanup();
});

type NoticeProps = React.ComponentProps<typeof SystemNotices>;

function noticeProps(overrides?: Partial<NoticeProps>): NoticeProps {
  return {
    status: "open",
    lagged: false,
    rateLimitRetriesExhausted: false,
    hasEverOpened: true,
    reconnecting: false,
    retryCount: 0,
    maxRetries: 7,
    manualReconnect: vi.fn(),
    ...overrides,
  };
}

const mount = (overrides?: Partial<NoticeProps>) => render(<SystemNotices {...noticeProps(overrides)} />);

describe("SystemNotices", () => {
  afterEach(() => {
    act(() => setServerDown(false));
    vi.useRealTimers();
  });

  // (label, props, what shows at once, what shows once a drop outlasts the grace)
  it.each<[string, Partial<NoticeProps>, string, string]>([
    ["first connect while starting", { status: "connecting", hasEverOpened: false, newSessionStarting: true }, "", ""],
    [
      "a first-connection retry while starting",
      { status: "error", reconnecting: true, retryCount: 1, hasEverOpened: false, newSessionStarting: true },
      "",
      "",
    ],
    [
      "a first connection",
      { status: "connecting", hasEverOpened: false },
      "Starting structured view…",
      "Starting structured view…",
    ],
    [
      "a drop once the Session has connected",
      { status: "error", reconnecting: true, retryCount: 1, hasEverOpened: true },
      "",
      "Reconnecting… new messages wait until it is back.",
    ],
    [
      "a drop while replay still delivers events",
      { status: "closed", reconnecting: true, retryCount: 2, hasEverOpened: true, replayedWhileDown: true },
      "",
      "Reconnecting… showing history fetched meanwhile; new messages wait until it is back.",
    ],
    [
      "a cap park with no limit snapshot",
      { rateLimitRetriesExhausted: true },
      "Auto-resume stopped: the same prompt was re-sent too many times without getting through. Send a new prompt.",
      "Auto-resume stopped: the same prompt was re-sent too many times without getting through. Send a new prompt.",
    ],
  ])("%s", (_label, overrides, atOnce, afterGrace) => {
    vi.useFakeTimers();
    const { container } = mount(overrides);
    expect(container.textContent).toBe(atOnce);
    act(() => vi.advanceTimersByTime(2000));
    expect(container.textContent).toBe(afterGrace);
    // No attempt count or countdown: neither says anything a user can act on.
    expect(container.textContent).not.toMatch(/\d+\/\d+|in \d+s/);
  });

  it("keeps quiet while the Daemon banner already says the connection is down", () => {
    vi.useFakeTimers();
    act(() => setServerDown(true));
    const { container } = mount({ status: "error", reconnecting: true, retryCount: 1, hasEverOpened: true });
    act(() => vi.advanceTimersByTime(2000));
    expect(container.textContent).toBe("");
    cleanup();
    const stopped = mount({ status: "closed", hasEverOpened: true, retryCount: 7 });
    expect(stopped.container.textContent).toBe("");
  });

  it.each([false, true])("offers recovery after retry exhaustion (previously connected: %s)", (hasEverOpened) => {
    const props = noticeProps({ status: "closed", hasEverOpened, retryCount: 7 });
    const starting = isNewSessionStarting(emptyAcpState(), props, "resuming", false);
    const { getByRole, queryByText } = render(<SystemNotices {...props} newSessionStarting={starting} />);
    expect(starting).toBe(false);
    expect(queryByText(hasEverOpened ? "Connection lost." : "Could not connect.")).not.toBeNull();
    fireEvent.click(getByRole("button", { name: "Reconnect" }));
    expect(props.manualReconnect).toHaveBeenCalledTimes(1);
  });
});
