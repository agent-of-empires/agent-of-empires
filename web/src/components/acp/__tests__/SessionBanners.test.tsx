// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { useReducer } from "react";

import { emptyAcpState, type AcpEvent } from "../../../lib/acpTypes";
import { reducer, type Action } from "../../../hooks/acpSession/reducer";
import { isNewSessionStarting } from "../startupWait";
import {
  ScheduledWakeupBanner,
  SessionBanners,
  SnoozedWorkerStoppedBanner,
  StartupWaitLine,
  TrashedWorkerStoppedBanner,
  WorkerRestartingBanner,
} from "../SessionBanners";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("SnoozedWorkerStoppedBanner", () => {
  it.each([
    ["2099-01-01T00:00:00Z", /2099|2098/],
    // Unparseable timestamps render raw instead of "Invalid Date".
    ["not-a-date", /not-a-date/],
  ])("renders the wake time for %s", (snoozedUntil, wake) => {
    render(<SnoozedWorkerStoppedBanner sessionId="abc-123" snoozedUntil={snoozedUntil} />);
    const text = screen.getByTestId("acp-snoozed-banner-abc-123").textContent;
    expect(text).toMatch(wake);
    expect(text).not.toContain("Invalid Date");
  });
});

describe("TrashedWorkerStoppedBanner", () => {
  it("renders the read-only trash notice without Restore when onRestore is omitted", () => {
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" />);
    expect(screen.getByTestId("acp-trashed-banner-sess-9")).toBeTruthy();
    expect(screen.getByText("Session in trash")).toBeTruthy();
    expect(screen.getByText(/read-only/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Restore" })).toBeNull();
  });

  it("disables Restore while pending so a double-click cannot fire twice", () => {
    const onRestore = vi.fn(() => new Promise<boolean>(() => {}));
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={onRestore} />);
    fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    const pending = screen.getByRole("button", { name: "Restoring…" }) as HTMLButtonElement;
    expect(pending.disabled).toBe(true);
    fireEvent.click(pending);
    expect(onRestore).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["resolves false", () => Promise.resolve(false)],
    ["rejects", () => Promise.reject(new Error("boom"))],
  ])("resets the pending state when restore %s", async (_label, impl) => {
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={vi.fn(impl)} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    });
    await waitFor(() =>
      expect((screen.getByRole("button", { name: "Restore" }) as HTMLButtonElement).disabled).toBe(false),
    );
  });
});

describe("WorkerRestartingBanner", () => {
  const GENERIC = "Restarting structured view worker";
  const UNRESPONSIVE = "Agent stopped responding to cancel";
  const ORPHANED = "Agent finished but didn't notify the daemon";
  it.each([
    [false, false, GENERIC],
    [true, false, UNRESPONSIVE],
    [false, true, ORPHANED],
    // Both can briefly be set during a cancel-escalation race; orphaned wins.
    [true, true, ORPHANED],
  ])("unresponsive=%s orphaned=%s renders %s", (agentUnresponsive, agentOrphaned, expected) => {
    const { container } = render(
      <WorkerRestartingBanner agentUnresponsive={agentUnresponsive} agentOrphaned={agentOrphaned} />,
    );
    for (const copy of [GENERIC, UNRESPONSIVE, ORPHANED]) {
      expect(container.textContent?.includes(copy)).toBe(copy === expected);
    }
  });
});

describe("ScheduledWakeupBanner", () => {
  it("shows Waking… once fired, then self-dismisses after the grace window", () => {
    vi.useFakeTimers();
    const { container } = render(
      <ScheduledWakeupBanner wakeAt={new Date(Date.now() - 1_000).toISOString()} reason="fallback" />,
    );
    expect(container.textContent).toContain("Waking…");
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(container.textContent).toBe("");
  });

  it("keeps the countdown while the wake is still in the future", () => {
    vi.useFakeTimers();
    const { container } = render(
      <ScheduledWakeupBanner wakeAt={new Date(Date.now() + 120_000).toISOString()} reason="fallback" />,
    );
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(container.textContent).toContain("Asleep until");
  });
});

describe("StartupWaitLine", () => {
  const line = () => screen.getByTestId("startup-wait-line");
  const reducedMotion = (reduce: boolean) =>
    vi.stubGlobal(
      "matchMedia",
      vi.fn((query: string) => ({ matches: reduce && query.includes("reduce"), media: query })),
    );
  afterEach(() => vi.unstubAllGlobals());

  it("names the Agent and counts up, then says it's still starting, with no duration", () => {
    vi.useFakeTimers();
    reducedMotion(false);
    render(<StartupWaitLine sessionId="s1" agent="Claude Code" canRestart />);
    expect(line().textContent).toContain("Starting Claude Code… You can type now.");
    expect(screen.getByTestId("startup-wait-elapsed").textContent).toBe("0:00");
    act(() => vi.advanceTimersByTime(6_000));
    expect(screen.getByTestId("startup-wait-elapsed").textContent).toBe("0:06");
    act(() => vi.advanceTimersByTime(9_000));
    expect(line().textContent).toContain("Still starting Claude Code…");
    expect(line().textContent).not.toContain("You can type now");
    expect(screen.getByTestId("startup-wait-elapsed").textContent).toBe("0:15");
    act(() => vi.advanceTimersByTime(60_000));
    expect(screen.getByTestId("startup-wait-elapsed").textContent).toBe("1:15");
    for (const promise of [/few seconds/, /minute/, /30 seconds/, /will send/]) {
      expect(line().textContent).not.toMatch(promise);
    }
    // The count isn't announced: nothing in the line is a live region.
    expect(line().querySelector("[aria-live], [role=status], [role=alert]")).toBeNull();
    expect(line().getAttribute("aria-live")).toBeNull();
  });

  it.each([
    ["animates the breathe frames", false],
    ["holds one visible frame under reduced motion", true],
  ])("%s", (_label, reduce) => {
    vi.useFakeTimers();
    reducedMotion(reduce);
    const { unmount } = render(<StartupWaitLine sessionId="s1" agent="Codex" canRestart />);
    const glyph = () => line().firstElementChild!.textContent;
    const first = glyph();
    act(() => vi.advanceTimersByTime(180 * 4));
    if (reduce) {
      expect(first).toBe("⣿");
      expect(glyph()).toBe("⣿");
      // Only the elapsed tick runs.
      expect(vi.getTimerCount()).toBe(1);
    } else {
      expect(glyph()).not.toBe(first);
      expect(vi.getTimerCount()).toBe(2);
    }
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each([
    ["a successful restart starts the count again", 200, "0:01", null],
    [
      "a failed restart keeps Restart and says why",
      409,
      "0:31",
      "Restart failed: Server returned 409. worker_not_stopped",
    ],
  ])("offers Restart once the handshake bound has passed: %s", async (_label, status, elapsedAfter, failure) => {
    vi.useFakeTimers();
    reducedMotion(true);
    const spawn = vi.fn().mockResolvedValue({
      ok: status === 200,
      status,
      text: () => Promise.resolve(status === 200 ? "" : "worker_not_stopped"),
    });
    vi.stubGlobal("fetch", spawn);
    const onRestarted = vi.fn();
    render(
      <StartupWaitLine sessionId="s1" agent="Claude Code" canRestart restartEpoch={3} onRestarted={onRestarted} />,
    );
    const restart = () => screen.queryByRole("button", { name: "Restart Claude Code" });
    act(() => vi.advanceTimersByTime(29_000));
    expect(restart()).toBeNull();
    act(() => vi.advanceTimersByTime(1_000));
    await act(async () => {
      fireEvent.click(restart()!);
    });
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(spawn.mock.calls[0]![0]).toBe("/api/sessions/s1/acp/restart");
    act(() => vi.advanceTimersByTime(1_000));
    expect(screen.getByTestId("startup-wait-elapsed").textContent).toBe(elapsedAfter);
    expect(restart() === null).toBe(failure === null);
    expect(screen.queryByTestId("startup-wait-restart-error")?.textContent ?? null).toBe(failure);
    expect(onRestarted.mock.calls).toEqual(failure === null ? [[3]] : []);
  });

  it("acknowledges a restart with the epoch it was requested in", async () => {
    vi.useFakeTimers();
    reducedMotion(true);
    let respond!: (res: unknown) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise((resolve) => (respond = resolve))),
    );
    const onRestarted = vi.fn();
    const line = (restartEpoch: number) => (
      <StartupWaitLine
        sessionId="s1"
        agent="Claude Code"
        canRestart
        restartEpoch={restartEpoch}
        onRestarted={onRestarted}
      />
    );
    const { rerender } = render(line(0));
    act(() => vi.advanceTimersByTime(30_000));
    fireEvent.click(screen.getByRole("button", { name: "Restart Claude Code" }));
    // The replacement is assigned before the restart request answers.
    rerender(line(1));
    await act(async () => respond({ ok: true, status: 200, text: () => Promise.resolve("") }));
    expect(onRestarted.mock.calls).toEqual([[0]]);
  });

  const frame = (seq: number, event: AcpEvent): Action => ({
    kind: "frame",
    frame: { session_id: "s1", seq, event },
  });
  /** The owner StructuredView gives SessionBanners, with a sent prompt waiting on the start and the restart held. */
  function renderOwner() {
    vi.useFakeTimers();
    reducedMotion(true);
    let respond!: (res: unknown) => void;
    // Only the restart answers; the startup error banner's own fetches stay pending.
    vi.stubGlobal(
      "fetch",
      vi.fn(
        (url: string) =>
          new Promise((resolve) => {
            if (url.endsWith("/acp/restart")) respond = resolve;
          }),
      ),
    );
    let dispatch!: (action: Action) => void;
    function Owner() {
      const [state, send] = useReducer(reducer, reducer(emptyAcpState(), frame(1, { UserPromptSent: { text: "hi" } })));
      dispatch = send;
      const connection = {
        status: "open",
        hasEverOpened: true,
        reconnecting: false,
        retryCount: 0,
        maxRetries: 7,
      } as const;
      return (
        <SessionBanners
          sessionId="s1"
          state={state}
          startingAgent={isNewSessionStarting(state, connection, "resuming", false) ? "Claude Code" : null}
          canRestart
          onRestarted={(epoch) => send({ kind: "restart_started", epoch })}
          onRestartFailed={(epoch, message) => send({ kind: "restart_failed", epoch, message })}
          acpWorkerState="resuming"
          trashedAt={null}
          archivedAt={null}
          snoozedUntil={null}
          dismissError={vi.fn()}
        />
      );
    }
    render(<Owner />);
    act(() => vi.advanceTimersByTime(30_000));
    fireEvent.click(screen.getByRole("button", { name: "Restart Claude Code" }));
    return {
      dispatch: (action: Action) => act(() => dispatch(action)),
      respond: (status: number, body: string) =>
        act(async () => respond({ ok: status < 400, status, text: () => Promise.resolve(body) })),
    };
  }
  const restartFailure = () => screen.queryByTestId("worker-stopped-restart-failure")?.textContent ?? null;

  it.each([
    [500, "restart spawn failed: boom"],
    // A 4xx can follow the stop too; the unmounted line is what says so.
    [409, "session changed during restart preflight; retry the restart"],
  ])("names a restart that failed with %i after its stop on the stopped banner", async (status, body) => {
    const owner = renderOwner();
    owner.dispatch(frame(2, { Stopped: { reason: "user_stopped" } }));
    await owner.respond(status, body);
    expect(screen.queryByTestId("startup-wait-line")).toBeNull();
    expect(screen.getByRole("button", { name: "Reconnect" })).toBeTruthy();
    expect(restartFailure()).toBe(`Restart failed: Server returned ${status}. ${body}`);
  });

  it("leaves a refusal before the stop on the line, not on a later stop's banner", async () => {
    const owner = renderOwner();
    await owner.respond(409, "worker_not_stopped: the previous worker has not finished stopping");
    expect(screen.getByTestId("startup-wait-restart-error").textContent).toContain("worker_not_stopped");
    // The user then stops the start themselves.
    owner.dispatch(frame(2, { Stopped: { reason: "user_stopped" } }));
    expect(screen.getByRole("button", { name: "Reconnect" })).toBeTruthy();
    expect(restartFailure()).toBeNull();
  });
});

describe("SessionBanners startup line", () => {
  it.each([
    ["offers Restart where the server allows it", true],
    ["keeps only the elapsed line on read-only and CityHall dashboards", false],
  ])("%s", (_label, canRestart) => {
    vi.useFakeTimers();
    render(
      <SessionBanners
        sessionId="s1"
        state={{ ...emptyAcpState(), turnActive: true }}
        startingAgent="Claude Code"
        canRestart={canRestart}
        acpWorkerState="running"
        trashedAt={null}
        archivedAt={null}
        snoozedUntil={null}
        dismissError={vi.fn()}
      />,
    );
    act(() => vi.advanceTimersByTime(31_000));
    expect(screen.getByTestId("startup-wait-elapsed").textContent).toBe("0:31");
    expect(screen.queryByTestId("startup-wait-restart") !== null).toBe(canRestart);
  });
});
