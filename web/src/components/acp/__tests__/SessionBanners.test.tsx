// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { useReducer } from "react";

import {
  applyEvent,
  applyReducedState,
  emptyAcpState,
  type AcpEvent,
  nativeInputAvailable,
} from "../../../lib/acpTypes";

import { reducer, type Action } from "../../../hooks/acpSession/reducer";

import { isNewSessionStarting } from "../startupWait";

import type { AcpState, Approval, Elicitation, IntegrationLostReason, ReducedState } from "../../../lib/acpTypes";

import { setServerDown } from "../../../lib/connectionState";

import { clearAcpCache, useAcpSession } from "../../../hooks/useAcpSession";

import {
  currentControlSnapshot,
  flushAsync,
  installAcpFakes,
  json,
  lastSocket,
  profileWrapper,
} from "../../../hooks/__tests__/acpHarness";

import { ApprovalCard } from "../ApprovalCard";

import { AskUserQuestionCard } from "../AskUserQuestionCard";

import {
  SessionBanners,
  ScheduledWakeupBanner,
  SnoozedWorkerStoppedBanner,
  StartupWaitLine,
  TrashedWorkerStoppedBanner,
  WorkerRestartingBanner,
} from "../SessionBanners";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("operation and interaction error banners", () => {
  it.each([
    undefined,
    null,
    {
      source: "local_prompt_result",
      operation: "prompt",
      runtime: "unknown",
      observed_at_ms: 100,
    },
  ] as const)("reveals the retained operation failure after an interaction retry succeeds (%j)", (evidence) => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s",
      seq: 1,
      event: { PromptRuntimeError: { message: "canonical operation failure", evidence } },
    });
    state = reducer(state, {
      kind: "error",
      message: "approval HTTP request failed",
      target: { kind: "approval", nonce: "pending-card" },
    });
    const props = {
      sessionId: "s",
      acpWorkerState: "running" as const,
      startingAgent: null,
      trashedAt: null,
      archivedAt: null,
      snoozedUntil: null,
      dismissError: vi.fn(),
    };
    const { rerender } = render(<SessionBanners {...props} state={state} />);
    expect(screen.getByText("approval HTTP request failed")).toBeTruthy();
    expect(screen.queryByText("canonical operation failure")).toBeNull();
    state = reducer(state, { kind: "approval_resolved_locally", nonce: "pending-card" });
    rerender(<SessionBanners {...props} state={state} />);
    expect(screen.getByText("canonical operation failure")).toBeTruthy();
    expect(screen.queryByText("approval HTTP request failed")).toBeNull();
    expect(state.lastErrorEvidence).toEqual(evidence ?? null);
  });
});

describe("interaction card retries through the session hook", () => {
  beforeEach(() => {
    localStorage.clear();
    clearAcpCache();
    setServerDown(false);
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    clearAcpCache();
    localStorage.clear();
  });

  for (const kind of ["approval", "elicitation"] as const) {
    for (const failure of ["http", "network"] as const) {
      it.each(["success", "already-resolved"] as const)(
        `${kind} ${failure} failure enables retry and reveals the canonical error after %s`,
        async (confirmation) => {
          const sessionId = "interaction-retry";
          const nonce = "pending-card";
          const path = kind === "approval" ? "approvals" : "elicitations";
          let attempts = 0;
          const calls = installAcpFakes((call) => {
            if (call.method !== "POST" || !call.url.endsWith(`/acp/${path}/${nonce}`)) return undefined;
            attempts += 1;
            if (attempts === 1) {
              if (failure === "network") throw new TypeError("offline request");
              return new Response("INTERACTION_HTTP_FAILURE", { status: 503 });
            }
            return confirmation === "success" ? json({}) : new Response(`No pending ${kind} ${nonce}`, { status: 404 });
          });
          let latest!: AcpState;
          function InteractionSession() {
            const acp = useAcpSession(sessionId);
            latest = acp.state;
            return (
              <>
                <SessionBanners
                  sessionId={sessionId}
                  state={acp.state}
                  acpWorkerState="running"
                  startingAgent={null}
                  trashedAt={null}
                  archivedAt={null}
                  snoozedUntil={null}
                  dismissError={acp.dismissError}
                />
                {acp.state.pendingApprovals.map((approval) => (
                  <ApprovalCard
                    key={approval.nonce}
                    approval={approval}
                    canDecide={acp.status === "open" && nativeInputAvailable(acp.state)}
                    onResolve={(decision, optionId) => acp.resolveApproval(approval.nonce, decision, optionId)}
                  />
                ))}
                {acp.state.pendingElicitations.map((elicitation) => (
                  <AskUserQuestionCard
                    key={elicitation.nonce}
                    elicitation={elicitation}
                    onResolve={(resolution) => acp.resolveElicitation(elicitation.nonce, resolution)}
                  />
                ))}
              </>
            );
          }
          render(<InteractionSession />, { wrapper: profileWrapper });
          await flushAsync();
          lastSocket().open();
          await flushAsync();
          lastSocket().message(currentControlSnapshot(latest, { ready: true }));
          const approval: Approval = {
            nonce,
            destructive: false,
            requested_at: "2026-01-01T00:00:00Z",
            tool_call: {
              id: "pending-tool",
              name: "Bash",
              kind: "execute",
              args_preview: "{}",
              started_at: "2026-01-01T00:00:00Z",
            },
          };
          const elicitation: Elicitation = {
            nonce,
            message: "Answer the question",
            tool_call_id: null,
            requested_at: "2026-01-01T00:00:00Z",
            resolved: null,
            questions: [
              {
                field_key: "answer",
                title: null,
                description: null,
                required: true,
                kind: "free_text",
                options: [],
                default: "yes",
              },
            ],
          };
          const canonical = "canonical operation failure";
          const evidence = {
            source: "local_prompt_result",
            operation: "prompt",
            runtime: "unknown",
            observed_at_ms: 100,
          } as const;
          const requested: ReducedState = {
            agent: "claude",
            model: null,
            mode: "Default",
            current_plan: null,
            in_flight_tool: null,
            pending_approvals: kind === "approval" ? [approval] : [],
            pending_elicitations: kind === "elicitation" ? [elicitation] : [],
            thinking: null,
            rate_limit: null,
            available_commands: [],
            available_modes: [],
            current_mode_id: null,
            turn_active: true,
            cancelling: false,
            compacting: false,
            operation_error: null,
            connection_ready: true,
          };
          const buttonName = kind === "approval" ? "Allow" : "Submit";
          lastSocket().message({ session_id: sessionId, seq: 1, event: { UserPromptSent: { text: "request A" } } });
          lastSocket().message({
            session_id: sessionId,
            seq: 2,
            event:
              kind === "approval" ? { ApprovalRequested: { approval } } : { ElicitationRequested: { elicitation } },
          });
          // Pending cards are daemon-folded control state, not a client fold
          // of the raw request event. Establish the actual card before failure.
          lastSocket().message({ kind: "reduced_state", seq: 2, state: requested });
          await flushAsync();
          expect((screen.getByRole("button", { name: buttonName }) as HTMLButtonElement).disabled).toBe(false);
          expect(latest.pendingApprovals.map((card) => card.nonce)).toEqual(kind === "approval" ? [nonce] : []);
          expect(latest.pendingElicitations.map((card) => card.nonce)).toEqual(kind === "elicitation" ? [nonce] : []);
          expect(attempts).toBe(0);
          lastSocket().message({
            session_id: sessionId,
            seq: 3,
            event: { PromptRuntimeError: { message: canonical, evidence } },
          });
          lastSocket().message({ session_id: sessionId, seq: 4, event: { Stopped: { reason: "prompt_failed" } } });
          lastSocket().message({
            kind: "reduced_state",
            seq: 4,
            state: { ...requested, turn_active: false, operation_error: evidence },
          });
          await flushAsync();
          expect(screen.getByText(canonical)).toBeTruthy();
          if (kind === "elicitation") {
            fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "retained answer" } });
          }
          fireEvent.click(screen.getByRole("button", { name: buttonName }));
          const noun = kind === "approval" ? "approval" : "question";
          const message =
            failure === "http"
              ? `Could not resolve ${noun} (503). INTERACTION_HTTP_FAILURE`
              : `Network error resolving ${noun}: offline request`;
          expect(await screen.findByText(message)).toBeTruthy();
          await waitFor(() =>
            expect((screen.getByRole("button", { name: buttonName }) as HTMLButtonElement).disabled).toBe(false),
          );
          if (kind === "approval") {
            for (const name of ["Always", "Deny"]) {
              expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(false);
            }
          }
          if (failure === "http") expect(screen.queryByText(/Network error resolving/)).toBeNull();
          expect(screen.queryByText(canonical)).toBeNull();
          expect(latest.lastError).toBe(canonical);
          expect(latest.lastErrorEvidence).toEqual(evidence);
          expect(latest.interactionError?.target).toEqual({ kind, nonce });
          fireEvent.click(screen.getByRole("button", { name: buttonName }));
          await waitFor(() => expect(screen.queryByRole("button", { name: buttonName })).toBeNull());
          expect(screen.getByText(canonical)).toBeTruthy();
          expect(screen.queryByText(message)).toBeNull();
          expect(latest.lastError).toBe(canonical);
          expect(latest.lastErrorEvidence).toEqual(evidence);
          expect(latest.interactionError).toBeNull();
          expect(latest.pendingApprovals).toHaveLength(0);
          expect(latest.pendingElicitations).toHaveLength(0);
          const resolutions = calls.filter(
            (call) => call.method === "POST" && call.url.endsWith(`/acp/${path}/${nonce}`),
          );
          expect(resolutions).toHaveLength(2);
          expect(resolutions[1]!.body).toBe(resolutions[0]!.body);
          expect(JSON.parse(resolutions[1]!.body!)).toEqual(
            kind === "approval" ? { decision: "Allow" } : { action: "accept", answers: { answer: "retained answer" } },
          );
        },
      );
    }
  }
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
    render(<StartupWaitLine sessionId="s1" agent="Claude Code" sentPending={false} queuedPending={false} canRestart />);
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
    const { unmount } = render(
      <StartupWaitLine sessionId="s1" agent="Codex" sentPending={false} queuedPending={false} canRestart />,
    );
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
      <StartupWaitLine
        sessionId="s1"
        agent="Claude Code"
        sentPending={false}
        queuedPending={false}
        canRestart
        restartEpoch={3}
        sentStopped={false}
        onRestarted={onRestarted}
      />,
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
    expect(onRestarted.mock.calls).toEqual(failure === null ? [[3, false]] : []);
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
        sentPending={false}
        queuedPending={false}
        canRestart
        restartEpoch={restartEpoch}
        sentStopped={false}
        onRestarted={onRestarted}
      />
    );
    const { rerender } = render(line(0));
    act(() => vi.advanceTimersByTime(30_000));
    fireEvent.click(screen.getByRole("button", { name: "Restart Claude Code" }));
    // The replacement is assigned before the restart request answers.
    rerender(line(1));
    await act(async () => respond({ ok: true, status: 200, text: () => Promise.resolve("") }));
    expect(onRestarted.mock.calls).toEqual([[0, false]]);
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
          onRestarted={(epoch, stoppedSent) => send({ kind: "restart_started", epoch, stoppedSent })}
          onRestartFailed={(epoch, message, stoppedSent) =>
            send({ kind: "restart_failed", epoch, message, stoppedSent })
          }
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
  const cost = () => screen.queryByTestId("startup-wait-restart-cost")?.textContent;
  const restartFailure = () => screen.queryByTestId("worker-stopped-restart-failure")?.textContent ?? null;

  it("keeps saying the sent message was stopped when the restart's stop lands first and the replacement stalls", async () => {
    const owner = renderOwner();
    // The Daemon answers the restart only after the old Worker stopped, so its stop arrives first.
    owner.dispatch(frame(2, { Stopped: { reason: "user_stopped" } }));
    expect(screen.queryByTestId("startup-wait-line")).toBeNull();
    await owner.respond(200, "");
    expect(screen.getByTestId("startup-wait-line")).toBeTruthy();
    // The replacement stalls too.
    act(() => vi.advanceTimersByTime(30_000));
    expect(cost()).toBe("The message you sent was stopped. Send it again once Claude Code is ready.");
    owner.dispatch(frame(3, { UserPromptSent: { text: "hi" } }));
    expect(cost()).toBe("Restarting stops the message you sent. Send it again once Claude Code is ready.");
  });

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
    expect(restartFailure()).toBe(
      `Restart failed: Server returned ${status}. ${body} The message you sent was stopped. Send it again once you reconnect.`,
    );
  });

  it("tells the startup error banner the sent message was stopped when the replacement fails before the restart answers", async () => {
    const owner = renderOwner();
    owner.dispatch(frame(2, { Stopped: { reason: "user_stopped" } }));
    owner.dispatch(
      frame(3, { AgentStartupError: { message: 'Failed to start structured view agent "claude": boom' } }),
    );
    await owner.respond(500, "restart spawn failed: boom");
    expect(screen.getByTestId("startup-error-banner-sent-stopped").textContent).toContain(
      "The message you sent was stopped.",
    );
    expect(restartFailure()).toBeNull();
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

  it.each([
    [
      "warns that a sent prompt will be stopped",
      true,
      false,
      "Restarting stops the message you sent. Send it again once Claude Code is ready.",
    ],
    [
      "says queued messages still go to the replacement",
      false,
      true,
      "Queued messages are sent once Claude Code is ready.",
    ],
    [
      "says both when a sent and a queued prompt wait",
      true,
      true,
      "Restarting stops the message you sent. Send it again once Claude Code is ready. Queued messages are sent once Claude Code is ready.",
    ],
    ["has no caption with nothing pending", false, false, ""],
  ])("Restart %s", (_label, sentPending, queuedPending, description) => {
    vi.useFakeTimers();
    reducedMotion(true);
    render(
      <StartupWaitLine
        sessionId="s1"
        agent="Claude Code"
        sentPending={sentPending}
        queuedPending={queuedPending}
        canRestart
        restartEpoch={0}
        sentStopped={false}
      />,
    );
    act(() => vi.advanceTimersByTime(30_000));
    const describedBy = screen.getByRole("button", { name: "Restart Claude Code" }).getAttribute("aria-describedby");
    expect(describedBy ? document.getElementById(describedBy)?.textContent : "").toBe(description);
    expect(screen.queryByTestId("startup-wait-restart-cost")?.textContent ?? "").toBe(description);
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
    expect(screen.queryByTestId("startup-wait-restart-cost") !== null).toBe(canRestart);
  });
});

describe("accepted integration-loss snapshot diagnostics", () => {
  const message = "Agent integration lost: hold overflow";
  const banners = (state: AcpState) => (
    <SessionBanners
      sessionId="snapshot-diagnostic"
      state={state}
      startingAgent={null}
      canRestart={false}
      acpWorkerState="running"
      trashedAt={null}
      archivedAt={null}
      snoozedUntil={null}
      dismissError={vi.fn()}
    />
  );
  const snapshot = (
    state: AcpState,
    reason: IntegrationLostReason | null,
    seq: number,
    receipt: Parameters<typeof currentControlSnapshot>[1] = { ready: false },
  ) => {
    const header = currentControlSnapshot(state, receipt, seq);
    const wire = { ...header.state, integration_lost: reason };
    return applyReducedState(state, wire, header.unchanged, header.seq);
  };

  beforeEach(() => setServerDown(false));

  it.each<[IntegrationLostReason, string]>([
    [{ kind: "contract_violation", detail: "invalid primitive epoch" }, "invalid primitive epoch"],
    [{ kind: "hold_overflow" }, "hold overflow"],
    [{ kind: "lease_overflow" }, "lease overflow"],
  ])("renders snapshot-only reason %j without a raw frame or native authority (%s)", (reason, detail) => {
    const state = snapshot(emptyAcpState(), reason, 20);
    render(banners(state));
    expect(screen.getByText(`Agent integration lost: ${detail}`)).toBeTruthy();
    expect(state).toMatchObject({
      connectionReady: false,
      connectionUnavailable: null,
      promptWakeEligible: false,
      nativeSnapshotSeq: 20,
    });
    expect(nativeInputAvailable(state)).toBe(false);
  });

  it.each<[string, Parameters<typeof currentControlSnapshot>[1]]>([
    ["Available", { ready: true }],
    ["governed wake", { ready: false, wakeEligible: true }],
    [
      "qualified native loss",
      {
        ready: false,
        unavailable: { source: "native", cause: "stdout_eof", phase: "established" },
      },
    ],
  ])("keeps integration diagnostics separate from the %s native receipt", (_label, receipt) => {
    const header = currentControlSnapshot(emptyAcpState(), receipt, 20);
    const baseline = applyReducedState(emptyAcpState(), header.state, [], header.seq);
    const state = snapshot(baseline, { kind: "hold_overflow" }, 20, receipt);
    render(banners(state));
    expect(screen.getByText(message)).toBeTruthy();
    expect({
      ready: state.connectionReady,
      unavailable: state.connectionUnavailable,
      wake: state.promptWakeEligible,
    }).toEqual({
      ready: baseline.connectionReady,
      unavailable: baseline.connectionUnavailable,
      wake: baseline.promptWakeEligible,
    });
    expect(nativeInputAvailable(state)).toBe(nativeInputAvailable(baseline));
  });

  it.each<["current reason" | "current clear", IntegrationLostReason | null]>([
    ["current reason", { kind: "hold_overflow" }],
    ["current clear", null],
  ])("protects the %s from covered raw loss/reset history and an older snapshot", (_label, reason) => {
    let state = snapshot(emptyAcpState(), reason, 20);
    const view = render(banners(state));
    const expectDiagnostic = () => {
      if (reason) expect(screen.getByText(message)).toBeTruthy();
      else expect(screen.queryByText(/Agent integration lost:/)).toBeNull();
      expect(nativeInputAvailable(state)).toBe(false);
    };
    expectDiagnostic();
    state = applyEvent(state, {
      session_id: "snapshot-diagnostic",
      seq: 5,
      event: { IntegrationLost: { reason: { kind: "contract_violation", detail: "older raw loss" } } },
    });
    view.rerender(banners(state));
    expectDiagnostic();
    state = applyEvent(state, {
      session_id: "snapshot-diagnostic",
      seq: 6,
      event: { UserPromptSent: { text: "older raw prompt" } },
    });
    view.rerender(banners(state));
    expectDiagnostic();
    expect(snapshot(state, { kind: "lease_overflow" }, 19)).toEqual(state);
    state = applyEvent(state, {
      session_id: "snapshot-diagnostic",
      seq: 21,
      event: { AcpSessionAssigned: { acp_session_id: "new-current-native" } },
    });
    view.rerender(banners(state));
    expect(screen.queryByText(/Agent integration lost:/)).toBeNull();
    expect(nativeInputAvailable(state)).toBe(false);
  });

  it("keeps raw integration diagnostics when a legacy snapshot omits the optional field", () => {
    const initial = applyEvent(emptyAcpState(), {
      session_id: "snapshot-diagnostic",
      seq: 1,
      event: { IntegrationLost: { reason: { kind: "hold_overflow" } } },
    });
    const header = currentControlSnapshot(initial, { ready: false }, 20);
    delete header.state.integration_lost;
    const state = applyReducedState(initial, header.state, [], header.seq);
    render(banners(state));
    expect(screen.getByText(message)).toBeTruthy();
    expect(nativeInputAvailable(state)).toBe(false);
  });

  it("clears only the current integration diagnostic while preserving unrelated errors", () => {
    let initial = applyEvent(emptyAcpState(), {
      session_id: "snapshot-diagnostic",
      seq: 1,
      event: { AgentStartupError: { message: "Unrelated startup diagnostic" } },
    });
    initial = applyEvent(initial, {
      session_id: "snapshot-diagnostic",
      seq: 2,
      event: { PromptRuntimeError: { message: "Retained operation diagnostic" } },
    });
    initial = reducer(initial, { kind: "error", message: "Retained callback failure" });
    const lost = snapshot(initial, { kind: "hold_overflow" }, 20);
    const view = render(banners(lost));
    expect(screen.getByText(message)).toBeTruthy();
    const cleared = snapshot(lost, null, 21);
    view.rerender(banners(cleared));
    expect(screen.queryByText(/Agent integration lost:/)).toBeNull();
    expect(cleared.startupError).toBe("Unrelated startup diagnostic");
    expect(cleared.lastError).toBe("Retained operation diagnostic");
    expect(cleared.interactionError).toEqual(initial.interactionError);
    expect(screen.getByText("Unrelated startup diagnostic")).toBeTruthy();
    expect(screen.getByText("Retained callback failure")).toBeTruthy();
    expect(nativeInputAvailable(cleared)).toBe(false);
  });
});
