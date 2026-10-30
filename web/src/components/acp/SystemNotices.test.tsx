// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { SystemNotices } from "./SystemNotices";
import { emptyAcpState } from "../../lib/acpTypes";
import { isNewSessionStarting } from "./startupWait";

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
    retryCountdown: 0,
    maxRetries: 7,
    manualReconnect: vi.fn(),
    ...overrides,
  };
}

const mount = (overrides?: Partial<NoticeProps>) => render(<SystemNotices {...noticeProps(overrides)} />);

describe("SystemNotices", () => {
  // The startup wait line covers a new Session's first connection; reconnect lines stay.
  it.each<[string, Partial<NoticeProps>, RegExp | null]>([
    ["first connect while starting", { status: "connecting", hasEverOpened: false, newSessionStarting: true }, null],
    ["first connect error while starting", { status: "error", hasEverOpened: false, newSessionStarting: true }, null],
    [
      "a first-connection retry while starting",
      { status: "error", reconnecting: true, retryCount: 1, hasEverOpened: false, newSessionStarting: true },
      null,
    ],
    ["a closed socket while starting", { status: "closed", hasEverOpened: false, newSessionStarting: true }, null],
    [
      "a retry once the Session has connected",
      { status: "error", reconnecting: true, retryCount: 1, hasEverOpened: true, newSessionStarting: true },
      /Structured view disconnected\. Reconnecting \(1\/7\)/,
    ],
    [
      "a retry while replay still delivers events",
      { status: "closed", reconnecting: true, retryCount: 2, hasEverOpened: true, replayedWhileDown: true },
      /Live updates unavailable, showing history fetched over HTTP\. Reconnecting \(2\/7\)/,
    ],
    [
      "a reconnect",
      { status: "connecting", hasEverOpened: true, newSessionStarting: true },
      /Reconnecting to structured view/,
    ],
    [
      "a disconnect",
      { status: "error", hasEverOpened: true, newSessionStarting: false },
      /Structured view reconnecting/,
    ],
    [
      "a cap park with no limit snapshot",
      { rateLimitRetriesExhausted: true },
      /Auto-resume stopped: the same prompt was re-sent too many times/,
    ],
  ])("%s", (_label, overrides, expected) => {
    const { container } = mount(overrides);
    if (expected) expect(container.textContent).toMatch(expected);
    else expect(container.textContent).toBe("");
  });

  it.each([false, true])("offers recovery after retry exhaustion (previously connected: %s)", (hasEverOpened) => {
    const props = noticeProps({ status: "closed", hasEverOpened, retryCount: 7 });
    const starting = isNewSessionStarting(emptyAcpState(), props, "resuming", false);
    const { getByRole, queryByText } = render(<SystemNotices {...props} newSessionStarting={starting} />);
    expect(starting).toBe(false);
    expect(
      queryByText(hasEverOpened ? "Connection lost. Auto-retry stopped." : "Could not connect. Auto-retry stopped."),
    ).not.toBeNull();
    expect(queryByText(/Starting|Retrying|Reconnecting/)).toBeNull();
    fireEvent.click(getByRole("button", { name: "Reconnect" }));
    expect(props.manualReconnect).toHaveBeenCalledOnce();
  });
});
