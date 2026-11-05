// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

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
    retryCountdown: 0,
    maxRetries: 7,
    manualReconnect: vi.fn(),
    ...overrides,
  };
}

const mount = (overrides?: Partial<NoticeProps>) => render(<SystemNotices {...noticeProps(overrides)} />);

describe("SystemNotices", () => {
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
