// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";

import { SystemNotices } from "./SystemNotices";
import {
  applyEvent,
  applyReducedState,
  canSubmitPromptIntent,
  emptyAcpState,
  mergeServerRows,
  revokeNativeAuthority,
  type AcpEvent,
  type AcpState,
} from "../../lib/acpTypes";
import { loadPersistedState, persistState } from "../../hooks/acpSession/stateCache";
import { currentControlSnapshot } from "../../../tests/helpers/acpControlSnapshot";
import { queuedReason } from "./queuedReason";
import { isNewSessionStarting } from "./startupWait";
import { setServerDown } from "../../lib/connectionState";

afterEach(() => {
  cleanup();
  localStorage.clear();
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

  it("keeps quota advice canonical across foreign history, hydration, and later current snapshots", () => {
    const id = "canonical-quota-notices";
    const snapshot = (state: AcpState, seq: number, rateLimitPark: "none" | "armed" | "exhausted" | null) => {
      const receipt = currentControlSnapshot(state, { ready: true, rateLimitPark }, seq);
      return applyReducedState(state, receipt.state, receipt.unchanged, receipt.seq);
    };
    let state = snapshot(emptyAcpState(), 20, "none");
    const view = render(<SystemNotices {...noticeProps()} />);
    const advice = () => {
      view.rerender(
        <SystemNotices
          {...noticeProps({ rateLimitRetriesExhausted: state.rateLimitRetriesExhausted === true && !state.rateLimit })}
        />,
      );
      return queuedReason({
        agent: "Claude",
        online: true,
        anyPending: false,
        waitingFor: null,
        rateLimitParked: state.rateLimitParked === true,
        rateLimit: state.rateLimit,
        rateLimitAutoResume: true,
        workerDown: false,
        workerStarting: false,
        cancelling: false,
        compacting: false,
        turnActive: false,
        steerable: false,
        firstText: "next task",
        soloAliases: [],
      });
    };
    const raw = (seq: number, event: AcpEvent) => {
      state = applyEvent(state, { session_id: id, seq, event });
    };
    const expectNoQuotaAdvice = () => {
      expect(advice()).toMatchObject({ reason: null, action: "send" });
      expect(view.queryByText(/Auto-resume stopped/)).toBeNull();
    };
    raw(5, { Stopped: { reason: "rate_limit_exhausted_retries" } });
    raw(6, { Stopped: { reason: "rate_limited" } });
    raw(21, { Stopped: { reason: "rate_limit_exhausted_retries" } });
    raw(22, { Stopped: { reason: "rate_limited" } });
    expectNoQuotaAdvice();
    expect(canSubmitPromptIntent(state)).toBe(true);
    state = {
      ...state,
      activity: mergeServerRows(state.activity, [
        { id: "historical-reply", kind: "message", text: "Retained historical answer", at: "2026-01-01T00:00:00Z" },
      ]),
    };

    state = snapshot(state, 23, "armed");
    expect(advice()).toMatchObject({ reason: "sends after the rate limit lifts", action: "send" });
    expect(view.queryByText(/Auto-resume stopped/)).toBeNull();
    state = snapshot(state, 24, "exhausted");
    expect(advice().reason).toBe("sends after the rate limit lifts");
    expect(view.queryByText(/Auto-resume stopped/)).not.toBeNull();
    raw(25, { UserPromptSent: { text: "foreign prompt" } });
    raw(26, { RateLimitAutoResumed: { resets_at: "2026-01-01T00:00:00Z" } });
    raw(27, { AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" } });
    expect(advice().reason).toBe("sends after the rate limit lifts");
    expect(view.queryByText(/Auto-resume stopped/)).not.toBeNull();

    persistState(id, state);
    state = loadPersistedState(id)!;
    expect(state.rateLimitParked).toBeNull();
    expect(state.rateLimitRetriesExhausted).toBeNull();
    expectNoQuotaAdvice();
    expect(canSubmitPromptIntent(state)).toBe(false);
    expect(state.activity.map((row) => row.text)).toContain("Retained historical answer");
    state = snapshot(state, 28, "exhausted");
    expect(advice().reason).toBe("sends after the rate limit lifts");
    expect(view.queryByText(/Auto-resume stopped/)).not.toBeNull();
    state = revokeNativeAuthority(state);
    expectNoQuotaAdvice();
    expect(state.rateLimitParked).toBeNull();
    state = snapshot(state, 29, null);
    expectNoQuotaAdvice();
    expect(state.rateLimitRetriesExhausted).toBeNull();
    expect(canSubmitPromptIntent(state)).toBe(true);
    raw(30, { Stopped: { reason: "rate_limit_exhausted_retries" } });
    raw(31, { Stopped: { reason: "rate_limited" } });
    expectNoQuotaAdvice();
    expect(state.rateLimitParked).toBeNull();
    expect(state.rateLimitRetriesExhausted).toBeNull();
    state = snapshot(state, 32, "none");
    expectNoQuotaAdvice();
    expect(state.rateLimitParked).toBe(false);
    state = snapshot(state, 31, "exhausted");
    expectNoQuotaAdvice();
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
