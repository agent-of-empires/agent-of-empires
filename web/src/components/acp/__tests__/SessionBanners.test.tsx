// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { applyEvent, emptyAcpState } from "../../../lib/acpTypes";
import type { AcpState, Approval, Elicitation, ReducedState } from "../../../lib/acpTypes";
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
import { reducer } from "../../../hooks/acpSession/reducer";
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
    render(<StartupWaitLine agent="Claude Code" />);
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
    const { unmount } = render(<StartupWaitLine agent="Codex" />);
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
});
