// Reducer tests for the structured view memory/recall feature.
//
// These cover the wire-protocol contract: the server publishes a
// UserPromptSent event before forwarding the prompt to the agent, the
// frontend's optimistic dispatch produces a placeholder activity row,
// and the reducer dedupes the two by promoting the placeholder's id
// to the seq-based form when the server echo arrives.
//
// If this dedupe regresses, the user will see every prompt twice in
// the conversation log on every reload.

import { describe, expect, it } from "vitest";

import {
  applyEvent,
  emptyAcpState,
  isTurnActive,
  normaliseTurnCounters,
  type AcpFrame,
  type AcpState,
  type ToolCall,
} from "./acpTypes";

function frame(seq: number, text: string): AcpFrame {
  return {
    session_id: "s-1",
    seq,
    event: { UserPromptSent: { text } },
  };
}

function withOptimisticPrompt(state: AcpState, text: string, id = "cmp-1"): AcpState {
  // Mirrors the optimistic dispatch in useAcpSession.sendPrompt: an overlay
  // row keyed by the minted prompt_id (never appended to the server-owned
  // `activity`), with `pendingUserPromptSeq` bumped so a subsequent server
  // echo carrying the same prompt_id doesn't double-count. See #1170 / #3173.
  const pendingUserPromptSeq = state.pendingUserPromptSeq + 1;
  return {
    ...state,
    optimisticRows: state.optimisticRows.concat({
      id,
      kind: "user_prompt",
      text,
      at: new Date().toISOString(),
    }),
    pendingUserPromptSeq,
    turnActive: pendingUserPromptSeq > state.lastStoppedSeq,
  };
}

describe("applyEvent / UserPromptSent (control state)", () => {
  // The transcript row is server-owned now (Tier 4); applyEvent only advances
  // the turn counter and applies the per-turn resets. The optimistic overlay
  // reconcile-by-prompt_id lives in the hook + acpTypes.reducer.test.ts.
  it("bumps the turn counter and marks the turn active, adding no activity row", () => {
    const next = applyEvent(emptyAcpState(), frame(1, "hi"));
    expect(next.activity).toHaveLength(0);
    expect(next.pendingUserPromptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
    expect(next.lastSeq).toBe(1);
  });

  it("clears startup/error flags so the new turn starts clean", () => {
    const stale: AcpState = {
      ...emptyAcpState(),
      startupError: "old error",
      lastError: "old action error",
      turnActive: false,
    };
    const next = applyEvent(stale, frame(1, "new prompt"));
    expect(next.startupError).toBeNull();
    expect(next.lastError).toBeNull();
    expect(next.turnActive).toBe(true);
  });

  it("is a no-op for a frame at or below lastSeq (returns the same ref)", () => {
    const seeded: AcpState = { ...emptyAcpState(), lastSeq: 3 };
    const next = applyEvent(seeded, frame(3, "dup"));
    expect(next).toBe(seeded);
  });
});

describe("applyEvent / UserDiffCommentsPrompt (#1123) (control state)", () => {
  function diffCommentsFrame(seq: number): AcpFrame {
    return {
      session_id: "s-1",
      seq,
      event: {
        UserDiffCommentsPrompt: {
          intro: "Take a look:",
          outro: "Please address these comments.",
          isMultiRepo: true,
          comments: [],
          assembledMarkdown: "Take a look:\n\n## Diff comments\n\n...\n",
        },
      },
    };
  }

  it("bumps the turn counter (the typed row itself is server-owned)", () => {
    const next = applyEvent(emptyAcpState(), diffCommentsFrame(1));
    expect(next.activity).toHaveLength(0);
    expect(next.pendingUserPromptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
  });

  it("applies the same per-turn resets as a plain prompt", () => {
    const stale: AcpState = {
      ...emptyAcpState(),
      startupError: "old error",
      lastError: "old action error",
      workerStopped: true,
      workerRestarting: true,
      agentUnresponsive: true,
      turnActive: false,
    };
    const next = applyEvent(stale, diffCommentsFrame(1));
    expect(next.startupError).toBeNull();
    expect(next.lastError).toBeNull();
    expect(next.workerStopped).toBe(false);
    expect(next.workerRestarting).toBe(false);
    expect(next.agentUnresponsive).toBe(false);
    expect(next.turnActive).toBe(true);
  });

  it("counts as a prior user turn so a later SessionContextReset arms the primer (#1123)", () => {
    let state = applyEvent(emptyAcpState(), diffCommentsFrame(1));
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "session/load failed: bad id" } },
    });
    expect(state.contextPrimerAvailable).toEqual({
      resetSeq: 2,
      reason: "session/load failed: bad id",
    });
  });
});

describe("applyEvent / AvailableCommandsUpdated", () => {
  it("populates availableCommands and replaces the prior list", () => {
    const f1: AcpFrame = {
      session_id: "s-1",
      seq: 1,
      event: {
        AvailableCommandsUpdated: {
          commands: [{ name: "help", description: "Show help", accepts_input: false }],
        },
      },
    };
    const s1 = applyEvent(emptyAcpState(), f1);
    expect(s1.availableCommands).toHaveLength(1);
    expect(s1.availableCommands[0].name).toBe("help");

    const f2: AcpFrame = {
      session_id: "s-1",
      seq: 2,
      event: {
        AvailableCommandsUpdated: {
          commands: [
            { name: "review", description: "Review PR", accepts_input: true },
            {
              name: "clear",
              description: "Clear context",
              accepts_input: false,
            },
          ],
        },
      },
    };
    const s2 = applyEvent(s1, f2);
    expect(s2.availableCommands.map((c) => c.name)).toEqual(["review", "clear"]);
    expect(s2.availableCommands[0].accepts_input).toBe(true);
  });
});

describe("applyEvent / ACP session id lifecycle", () => {
  it("AcpSessionAssigned is a no-op for the conversation surface", () => {
    const before = emptyAcpState();
    const after = applyEvent(before, {
      session_id: "s-1",
      seq: 1,
      event: { AcpSessionAssigned: { acp_session_id: "uuid-1234" } },
    });
    // Seq advanced; no activity row appended; usage untouched.
    expect(after.lastSeq).toBe(1);
    expect(after.activity).toEqual([]);
    expect(after.sessionUsage).toBeNull();
  });

  it("SessionContextReset clears stale usage and arms the primer after a prior prompt", () => {
    // The context_reset transcript row is server-owned (Tier 4); applyEvent
    // clears the usage/baseline and arms the one-shot primer affordance,
    // gated on a prior user turn via pendingUserPromptSeq.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UsageUpdated: { usage: { used: 75000, size: 200000 } } },
    });
    expect(state.sessionUsage?.used).toBe(75000);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "hi" } },
    });

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        SessionContextReset: { reason: "session/load failed: bad id" },
      },
    });
    expect(state.sessionUsage).toBeNull();
    expect(state.contextPrimerAvailable).toEqual({
      resetSeq: 3,
      reason: "session/load failed: bad id",
    });
  });

  it("SessionContextReset uses a fallback primer reason when reason is empty", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "" } },
    });
    expect(state.contextPrimerAvailable?.reason.length).toBeGreaterThan(0);
  });

  it("SessionContextReset is silent on a session with no prior user prompt", () => {
    // 0-message session: agent never persisted a transcript, so session/load
    // failing on the next spawn is expected. Don't arm the primer.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UsageUpdated: { usage: { used: 100, size: 200000 } } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        SessionContextReset: { reason: "session/load failed: bad id" },
      },
    });
    // Usage still cleared (defensive — should already be safe to drop).
    expect(state.sessionUsage).toBeNull();
    expect(state.contextPrimerAvailable).toBeNull();
    expect(state.lastSeq).toBe(2);
  });

  it("SessionContextReset that arrives BEFORE the first prompt stays hidden after later prompts", () => {
    // Replay order: reset@2, then prompt@3. The reset must NOT appear
    // above the prompt later — applyEvent processes events in seq order
    // and decides based on what's been seen so far.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UsageUpdated: { usage: { used: 100, size: 200000 } } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "session/load failed" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { UserPromptSent: { text: "hi" } },
    });
    expect(state.activity.some((r) => r.kind === "context_reset")).toBe(false);
  });

  it("SessionContextReset with prior prompt sets contextPrimerAvailable (#1004)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "do a thing" } },
    });
    expect(state.contextPrimerAvailable).toBeNull();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "load failed: bad id" } },
    });
    expect(state.contextPrimerAvailable).toEqual({
      resetSeq: 2,
      reason: "load failed: bad id",
    });
  });

  it("SessionContextReset without prior prompt does not set contextPrimerAvailable", () => {
    const state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { SessionContextReset: { reason: "load failed" } },
    });
    expect(state.contextPrimerAvailable).toBeNull();
  });

  it("codex /new driven reset drops the context tracker to the post-reset baseline (#2979)", () => {
    // The server-side reset for a codex `/new` publishes UserPromptSent +
    // SessionCleared, then the live worker's fresh session/new emits
    // SessionContextReset + AcpSessionAssigned + Stopped(session_reset).
    // The tracker must not hold the pre-/new usage across that boundary,
    // and the fresh session's first UsageUpdated is the new baseline.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UsageUpdated: { usage: { used: 75000, size: 200000 } } },
    });
    expect(state.sessionUsage?.used).toBe(75000);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "/new" } },
    });
    expect(state.turnActive).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: "SessionCleared",
    });
    expect(state.sessionUsage).toBeNull();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: {
        SessionContextReset: { reason: "conversation cleared; the agent started a fresh session" },
      },
    });
    // The fresh ACP session restarts agent-side accounting at zero, so
    // the per-clear cost baseline no longer maps onto incoming values.
    expect(state.sessionUsage).toBeNull();
    expect(state.usageBaseline).toBeNull();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 5,
      event: { AcpSessionAssigned: { acp_session_id: "fresh-uuid" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 6,
      event: { Stopped: { reason: "session_reset" } },
    });
    // The clear command's synthetic turn is closed; composer unlocks.
    expect(state.turnActive).toBe(false);
    // The reset boundary counts as the turn's output: no spurious
    // "Command produced no output." row under the divider.
    expect(state.activity.some((r) => r.kind === "empty_output")).toBe(false);
    // The fresh session's first usage report is the post-reset baseline,
    // not the pre-/new 75k the tracker used to hold (#2979).
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 7,
      event: { UsageUpdated: { usage: { used: 1200, size: 200000 } } },
    });
    expect(state.sessionUsage?.used).toBe(1200);
  });

  it("UserPromptSent clears contextPrimerAvailable (one-shot affordance)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "first" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "load failed" } },
    });
    expect(state.contextPrimerAvailable).not.toBeNull();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { UserPromptSent: { text: "second" } },
    });
    expect(state.contextPrimerAvailable).toBeNull();
  });
});

describe("applyEvent / Stopped user_stopped", () => {
  it("sets workerStopped on reason=user_stopped and clears turnActive", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "long task" } },
    });
    expect(state.turnActive).toBe(true);
    expect(state.workerStopped).toBe(false);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "user_stopped" } },
    });
    expect(state.workerStopped).toBe(true);
    expect(state.turnActive).toBe(false);
  });

  it("does NOT set workerStopped on reason=prompt_complete", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.workerStopped).toBe(false);
  });

  it("clears workerStopped on the next UserPromptSent", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "user_stopped" } },
    });
    expect(state.workerStopped).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "back online" } },
    });
    expect(state.workerStopped).toBe(false);
  });

  it("clears workerStopped on AcpSessionAssigned (manual reconnect succeeded)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "user_stopped" } },
    });
    expect(state.workerStopped).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AcpSessionAssigned: { acp_session_id: "abc-123" } },
    });
    expect(state.workerStopped).toBe(false);
  });
});

describe("applyEvent / Stopped restart_pending", () => {
  it("sets workerRestarting (not workerStopped) on reason=restart_pending", () => {
    const state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "restart_pending" } },
    });
    expect(state.workerRestarting).toBe(true);
    expect(state.workerStopped).toBe(false);
    expect(state.turnActive).toBe(false);
  });

  it("clears workerRestarting on AcpSessionAssigned (reconciler auto-respawn finished)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "restart_pending" } },
    });
    expect(state.workerRestarting).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AcpSessionAssigned: { acp_session_id: "fresh-id" } },
    });
    expect(state.workerRestarting).toBe(false);
  });

  it("user_stopped → restart_pending transitions cleanly", () => {
    // Edge case: user runs `aoe acp stop`, then realises they meant
    // `restart`. The two reasons must not pile up — restart_pending
    // wins because it's the most recent signal from the daemon.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "user_stopped" } },
    });
    expect(state.workerStopped).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "restart_pending" } },
    });
    expect(state.workerStopped).toBe(false);
    expect(state.workerRestarting).toBe(true);
  });
});

describe("applyEvent / Stopped idle_auto_stop (#1689)", () => {
  it("sets workerIdleStopped (not workerStopped) on reason=idle_auto_stop", () => {
    const state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "idle_auto_stop" } },
    });
    expect(state.workerIdleStopped).toBe(true);
    // Crucially NOT a user stop: no reconnect banner, composer stays open.
    expect(state.workerStopped).toBe(false);
    expect(state.workerRestarting).toBe(false);
    expect(state.turnActive).toBe(false);
  });

  it("clears workerIdleStopped on the next UserPromptSent (the prompt woke it)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "idle_auto_stop" } },
    });
    expect(state.workerIdleStopped).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "wake up" } },
    });
    expect(state.workerIdleStopped).toBe(false);
  });

  it("clears workerIdleStopped on AcpSessionAssigned (respawn handshake landed)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "idle_auto_stop" } },
    });
    expect(state.workerIdleStopped).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AcpSessionAssigned: { acp_session_id: "fresh-id" } },
    });
    expect(state.workerIdleStopped).toBe(false);
  });
});

describe("applyEvent / WakeupScheduled lifecycle", () => {
  it("user-typed prompt mid-wait keeps the pending wakeup", () => {
    // Regression for #1091: a user-typed follow-up during the wait
    // is NOT the wake firing. Reducer must keep `nextWakeupAt` when
    // the scheduled time is still in the future.
    const future = new Date(Date.now() + 95_000).toISOString();
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { WakeupScheduled: { at: future, reason: "test wake" } },
    });
    expect(state.nextWakeupAt).toBe(future);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "btw, ping me when you wake" } },
    });
    expect(state.nextWakeupAt).toBe(future);
    expect(state.nextWakeupReason).toBe("test wake");
  });

  it("prompt after wakeup `at` clears the pending wakeup", () => {
    // The self-fired prompt from /loop arrives once the scheduled
    // moment has passed; that's the genuine wake-fired signal.
    const past = new Date(Date.now() - 5_000).toISOString();
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { WakeupScheduled: { at: past, reason: "test wake" } },
    });
    expect(state.nextWakeupAt).toBe(past);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "Wake-up fired. Confirm." } },
    });
    expect(state.nextWakeupAt).toBeNull();
    expect(state.nextWakeupReason).toBeNull();
  });
});

describe("applyEvent / MonitorArmed lifecycle", () => {
  it("MonitorArmed sets the monitoring badge with its description", () => {
    const state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { MonitorArmed: { description: "clippy passes" } },
    });
    expect(state.monitorArmed).toBe(true);
    expect(state.monitorDescription).toBe("clippy passes");
  });

  it("persists across agent activity with no user prompt", () => {
    // A monitor firing re-invokes the agent with activity but never a
    // UserPromptSent, so the badge must survive the resumed turn.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { MonitorArmed: { description: "build" } },
    });
    state = applyEvent(state, { session_id: "s-1", seq: 2, event: "ThinkingStarted" });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { AgentMessageChunk: { text: "resuming" } },
    });
    expect(state.monitorArmed).toBe(true);
  });

  it("clears on the next user prompt (the user takes over)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { MonitorArmed: { description: "watch" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { UserPromptSent: { text: "stop watching" } },
    });
    expect(state.monitorArmed).toBe(false);
    expect(state.monitorDescription).toBeNull();
  });

  it("persists on the arming turn's Stopped while the monitor is still pending", () => {
    // Arm, then end the turn with no post-arm tool work: the monitor has not
    // fired yet, the badge must stay up. See #2325.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { MonitorArmed: { description: "watch" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.monitorArmed).toBe(true);
  });

  it.each(["prompt_complete", "agent_idle"])(
    "clears once the monitor fired (tool started after arm) and the turn ends with %s",
    (reason) => {
      // The fire makes the agent act (a tool call after the arm); the badge
      // then retires on the next Stopped, covering both the in-band
      // (prompt_complete) and between-prompt (agent_idle) shapes. See #2325.
      let state = applyEvent(emptyAcpState(), {
        session_id: "s-1",
        seq: 1,
        event: { MonitorArmed: { description: "watch" } },
      });
      state = applyEvent(state, {
        session_id: "s-1",
        seq: 2,
        event: {
          ToolCallStarted: {
            tool_call: {
              id: "tc-1",
              name: "Read File",
              kind: "read",
              args_preview: "{}",
              started_at: new Date().toISOString(),
            },
          },
        },
      });
      // Fired-work seen but turn not ended yet: badge still up.
      expect(state.monitorArmed).toBe(true);
      state = applyEvent(state, {
        session_id: "s-1",
        seq: 3,
        event: { Stopped: { reason } },
      });
      expect(state.monitorArmed).toBe(false);
      expect(state.monitorDescription).toBeNull();
    },
  );
});

describe("applyEvent / CancelRequested lifecycle (#1727)", () => {
  function startedTurn() {
    // A turn must be active for cancelling to be meaningful.
    return applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "do a thing" } },
    });
  }

  it("CancelRequested sets cancelling + the escalation deadline", () => {
    const at = new Date(Date.now() + 10_000).toISOString();
    const state = applyEvent(startedTurn(), {
      session_id: "s-1",
      seq: 2,
      event: { CancelRequested: { escalates_at: at } },
    });
    expect(state.cancelling).toBe(true);
    expect(state.cancelEscalatesAt).toBe(at);
    // Turn is still active: CancelRequested is not a Stopped.
    expect(state.turnActive).toBe(true);
  });

  it("any Stopped clears the cancelling state", () => {
    const at = new Date(Date.now() + 10_000).toISOString();
    let state = applyEvent(startedTurn(), {
      session_id: "s-1",
      seq: 2,
      event: { CancelRequested: { escalates_at: at } },
    });
    expect(state.cancelling).toBe(true);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { Stopped: { reason: "user_forced" } },
    });
    expect(state.cancelling).toBe(false);
    expect(state.cancelEscalatesAt).toBeNull();
    expect(state.turnActive).toBe(false);
  });

  it("a fresh user prompt clears a stale cancelling flag", () => {
    const at = new Date(Date.now() + 10_000).toISOString();
    let state = applyEvent(startedTurn(), {
      session_id: "s-1",
      seq: 2,
      event: { CancelRequested: { escalates_at: at } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { UserPromptSent: { text: "next turn" } },
    });
    expect(state.cancelling).toBe(false);
    expect(state.cancelEscalatesAt).toBeNull();
  });

  it("replay reconstructs cancelling from the event stream", () => {
    // REST replay applies the same ordered events; cancelling must
    // survive a from-scratch rebuild, not depend on a local timer.
    const at = new Date(Date.now() + 10_000).toISOString();
    const frames = [
      { session_id: "s-1", seq: 1, event: { UserPromptSent: { text: "go" } } },
      {
        session_id: "s-1",
        seq: 2,
        event: { CancelRequested: { escalates_at: at } },
      },
    ];
    let state = emptyAcpState();
    for (const f of frames) state = applyEvent(state, f);
    expect(state.cancelling).toBe(true);
    expect(state.cancelEscalatesAt).toBe(at);
  });
});

describe("applyEvent / SessionCleared", () => {
  // /clear wipes the model's memory. The reducer appends a divider row
  // so the renderer can fold pre-clear turns behind a disclosure
  // (#1101), and resets only the per-turn / in-flight fields the
  // cleared context invalidates. Capability caches (slash commands,
  // modes) are preserved because claude-agent-sdk caches them at
  // Query init and does not rotate them on /clear (#1128).
  it("resets per-turn state but preserves capability caches (#1128)", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      availableCommands: [{ name: "foo", description: "", accepts_input: false }],
      availableModes: [{ id: "m1", name: "Mode One" }],
      currentModeId: "m1",
      plan: {
        plan_id: "p-1",
        version: 1,
        steps: [{ id: "s-1", title: "step", status: "Pending" }],
      },
      mode: "Plan",
      pendingApprovals: [
        {
          nonce: "n-1",
          tool_call: {
            id: "tc-1",
            name: "Bash",
            kind: "execute",
            args_preview: "ls",
            started_at: new Date().toISOString(),
          },
          destructive: false,
          requested_at: new Date().toISOString(),
        },
      ],
      sessionUsage: { used: 10, size: 200_000 },
    };
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 7,
      event: "SessionCleared",
    });
    // Per-turn / in-flight state cleared:
    expect(next.plan).toBeNull();
    expect(next.mode).toBe("Default");
    expect(next.pendingApprovals).toEqual([]);
    expect(next.sessionUsage).toBeNull();
    // Capability caches preserved (slash palette + mode picker keep
    // working after /clear):
    expect(next.availableCommands).toEqual(seeded.availableCommands);
    expect(next.availableModes).toEqual(seeded.availableModes);
    expect(next.currentModeId).toBe("m1");
  });
});

describe("applyEvent / ConversationCompacted", () => {
  // /compact is NOT memory loss: the model retains continuity through
  // the summary. The primer banner (which nudges the user to pre-fill
  // a recap) is therefore inappropriate here, so this event variant
  // exists as a separate signal from SessionContextReset and leaves
  // contextPrimerAvailable alone. See #1109.
  it("drops the stale usage snapshot (the compacted divider row is server-owned)", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      sessionUsage: { used: 100, size: 200_000 },
    };
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 9,
      event: "ConversationCompacted",
    });
    expect(next.activity).toHaveLength(0);
    expect(next.sessionUsage).toBeNull();
  });

  it("does not arm the primer banner", () => {
    // Regression: /compact previously routed through SessionContextReset
    // and the primer banner offered to pre-fill duplicate content the
    // model already had summarised. Verify the new variant doesn't
    // re-introduce that behaviour.
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 3,
      event: "ConversationCompacted",
    });
    expect(next.contextPrimerAvailable).toBeNull();
  });
});

describe("applyEvent / ConversationCompactionStarted (#3219)", () => {
  // The adapter goes silent for 90 to 170 seconds between the two
  // markers, so the phase has to be latched from an event rather than
  // inferred from the absence of frames. It clears on exactly two
  // events; `Stopped` is the self-healing one.
  const started = (seq: number): AcpFrame => ({
    session_id: "s-1",
    seq,
    event: "ConversationCompactionStarted",
  });

  it("latches the phase without adding a transcript row", () => {
    // The visible "Compacting..." chunk is already its own row; this
    // event is state only.
    const next = applyEvent(emptyAcpState(), started(4));
    expect(next.compacting).toBe(true);
    expect(next.activity).toHaveLength(0);
  });

  it.each([
    { label: "the completion marker", event: "ConversationCompacted" as const, expected: false },
    {
      label: "a clean Stopped",
      event: { Stopped: { reason: "prompt_complete" } } as const,
      expected: false,
    },
    {
      label: "a cancelled Stopped",
      event: { Stopped: { reason: "cancelled" } } as const,
      expected: false,
    },
    // The regression guard for the clear that must NOT exist:
    // applyNewTurnResets runs on every server-confirmed UserPromptSent,
    // including a follow-up confirmed inside the silent window. Clearing
    // there would relabel the spinner and re-arm the Force-end-turn
    // hatch while the compaction is still running.
    {
      label: "a mid-compaction UserPromptSent",
      event: { UserPromptSent: { text: "also check the tests" } } as const,
      expected: true,
    },
    { label: "ordinary streaming", event: "ThinkingStarted" as const, expected: true },
  ])("$label leaves compacting=$expected", ({ event, expected }) => {
    const latched = applyEvent(emptyAcpState(), started(1));
    expect(latched.compacting).toBe(true);
    const next = applyEvent(latched, { session_id: "s-1", seq: 2, event });
    expect(next.compacting).toBe(expected);
  });
});

describe("applyEvent / usageBaseline (#1354)", () => {
  // /clear and /compact do not rotate the underlying ACP session, so
  // claude-agent-acp keeps reporting session-lifetime cumulative cost
  // via UsageUpdate. The reducer captures a baseline at each boundary
  // and subtracts it from incoming UsageUpdate.cost so the composer
  // footer reads "since the most recent boundary."
  it("SessionCleared captures the cumulative cost as the baseline", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.42, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.42, 6);
    expect(state.usageBaseline).toBeNull();

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.sessionUsage).toBeNull();
    expect(state.usageBaseline?.cost).toBeCloseTo(0.42, 6);
  });

  it("UsageUpdated after /clear subtracts the baseline from cumulative cost", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.42, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        UsageUpdated: {
          usage: {
            used: 5_000,
            size: 200_000,
            cost: { amount: 0.49, currency: "USD" },
          },
        },
      },
    });
    // Cost is delta since clear; `used` and `size` flow through raw
    // (the agent already reports post-clear context size).
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.07, 6);
    expect(state.sessionUsage?.cost?.currency).toBe("USD");
    expect(state.sessionUsage?.used).toBe(5_000);
    expect(state.sessionUsage?.size).toBe(200_000);
  });

  it("/clear with no prior usage leaves the next UsageUpdate untouched", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBe(0);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        UsageUpdated: {
          usage: {
            used: 1_000,
            size: 200_000,
            cost: { amount: 0.05, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.05, 6);
  });

  it("repeated /clear accumulates the baseline to the true cumulative", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.1, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        UsageUpdated: {
          usage: {
            used: 4_000,
            size: 200_000,
            cost: { amount: 0.15, currency: "USD" },
          },
        },
      },
    });
    // Delta since first clear is 0.05.
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.05, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: "SessionCleared",
    });
    // Baseline is now the true cumulative (0.15), not the displayed
    // delta (0.05).
    expect(state.usageBaseline?.cost).toBeCloseTo(0.15, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 5,
      event: {
        UsageUpdated: {
          usage: {
            used: 2_000,
            size: 200_000,
            cost: { amount: 0.18, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.03, 6);
  });

  it("ConversationCompacted captures the baseline the same way as /clear", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 20_000,
            size: 200_000,
            cost: { amount: 0.3, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "ConversationCompacted",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.3, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        UsageUpdated: {
          usage: {
            used: 1_000,
            size: 200_000,
            cost: { amount: 0.32, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.02, 6);
  });

  it("AgentSwitched clears the baseline so the new backend starts at zero", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.42, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.42, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" },
      },
    });
    expect(state.usageBaseline).toBeNull();
    // The new agent reports its own cumulative starting at zero; no
    // subtraction should happen.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: {
        UsageUpdated: {
          usage: {
            used: 500,
            size: 200_000,
            cost: { amount: 0.01, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.01, 6);
  });

  it("SessionContextReset clears the baseline (new ACP session starts at zero)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.2, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.2, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { SessionContextReset: { reason: "session/load failed" } },
    });
    expect(state.usageBaseline).toBeNull();
  });

  it("UsageUpdated with no cost field is a no-op for the baseline", () => {
    // Codex / opencode / gemini adapters do not currently report cost.
    // The reducer must not crash and must not invent a cost value.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "SessionCleared",
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        UsageUpdated: { usage: { used: 100, size: 200_000 } },
      },
    });
    expect(state.sessionUsage?.cost ?? null).toBeNull();
    expect(state.sessionUsage?.used).toBe(100);
  });

  it("compact after /clear stacks the baseline onto the prior cumulative", () => {
    // Baseline carries across boundaries: /clear stashes the agent's
    // cumulative, then /compact must capture the still-cumulative value
    // (displayed delta plus the existing baseline), not just the
    // delta-since-clear. Otherwise the second boundary would
    // under-subtract from subsequent UsageUpdate frames.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.1, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.1, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        UsageUpdated: {
          usage: {
            used: 5_000,
            size: 200_000,
            cost: { amount: 0.15, currency: "USD" },
          },
        },
      },
    });
    // Displayed delta after /clear is 0.05.
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.05, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: "ConversationCompacted",
    });
    // Baseline at compact must be the true agent cumulative (0.15),
    // i.e. previous baseline 0.10 plus displayed delta 0.05.
    expect(state.usageBaseline?.cost).toBeCloseTo(0.15, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 5,
      event: {
        UsageUpdated: {
          usage: {
            used: 2_000,
            size: 200_000,
            cost: { amount: 0.17, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.02, 6);
  });

  it("UsageUpdated with baseline set but no incoming cost passes the usage through raw", () => {
    // Branch coverage: baseline-set + missing cost should hit the else
    // arm without crashing on the absent cost field, and store the raw
    // usage so used / size still surface in the footer.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.1, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.1, 6);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        UsageUpdated: { usage: { used: 1_000, size: 200_000 } },
      },
    });
    expect(state.sessionUsage?.used).toBe(1_000);
    expect(state.sessionUsage?.cost ?? null).toBeNull();
    // Baseline persists across a no-cost frame; the next cost-bearing
    // frame still subtracts correctly.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: {
        UsageUpdated: {
          usage: {
            used: 1_500,
            size: 200_000,
            cost: { amount: 0.12, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(0.02, 6);
  });

  it("clamps cost to zero if the agent ever reports a smaller cumulative than the baseline", () => {
    // Defensive: an upstream ACP-session restart could reset the
    // adapter's cumulative below the captured baseline. The reducer
    // must clamp at zero rather than display a negative dollar figure.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.5, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        UsageUpdated: {
          usage: {
            used: 100,
            size: 200_000,
            cost: { amount: 0.1, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBe(0);
  });
});

describe("applyEvent / AgentSwitched", () => {
  // Structured view hand-off (#1282) moves the session from one ACP backend
  // to another. Reducer must drop everything tied to the prior
  // backend so the UI doesn't show Claude's usage bar / mode pills /
  // in-flight tool while talking to Codex.
  it("clears prior-backend transient state and records the handoff", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      agent: "claude",
      rateLimit: {
        status: "limited",
        resets_at: "2099-01-01T00:00:00Z",
        kind: "rate_limit",
      },
      inFlightTool: {
        id: "t-1",
        name: "Read",
        kind: "read",
        args_preview: "{}",
        started_at: new Date().toISOString(),
      },
      thinking: true,
      sessionUsage: { used: 100, size: 200_000 },
      availableCommands: [{ name: "/clear", description: "wipe context", accepts_input: false }],
      availableModes: [{ id: "m1", name: "Default" }],
      currentModeId: "m1",
      mode: "Plan",
    };
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 11,
      event: {
        AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" },
      },
    });
    expect(next.agent).toBe("codex");
    expect(next.rateLimit).toBeNull();
    expect(next.inFlightTool).toBeNull();
    expect(next.thinking).toBe(false);
    expect(next.sessionUsage).toBeNull();
    expect(next.availableCommands).toEqual([]);
    expect(next.availableModes).toEqual([]);
    expect(next.currentModeId).toBeNull();
    expect(next.mode).toBe("Default");
    expect(next.lastAgentSwitch).toMatchObject({
      from: "claude",
      to: "codex",
      reason: "rate_limited",
    });
    // The transcript divider row is server-owned (Tier 4); applyEvent adds none.
    expect(next.activity).toHaveLength(0);
  });

  it("does not double-apply on replay", () => {
    const first = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 5,
      event: {
        AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" },
      },
    });
    const second = applyEvent(first, {
      session_id: "s-1",
      seq: 5, // same seq; reducer must drop.
      event: {
        AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" },
      },
    });
    expect(second).toBe(first);
  });

  // The supervisor emits Stopped { user_stopped } from the prior
  // backend's shutdown immediately before AgentSwitched. That flips
  // workerStopped (and possibly agentUnresponsive) on. Without an
  // explicit clear in this reducer the user sees a "worker stopped /
  // reconnecting" banner stacked on top of the freshly switched
  // session during the new agent's session/new handshake, which can
  // take several seconds before AcpSessionAssigned clears it.
  it("clears stale worker-stopped flags from the prior backend shutdown", () => {
    const seeded: AcpState = {
      ...emptyAcpState(),
      agent: "claude",
      workerStopped: true,
      workerRestarting: true,
      agentUnresponsive: true,
    };
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 13,
      event: {
        AgentSwitched: { from: "claude", to: "codex", reason: "rate_limited" },
      },
    });
    expect(next.workerStopped).toBe(false);
    expect(next.workerRestarting).toBe(false);
    expect(next.agentUnresponsive).toBe(false);
  });
});

describe("turnActive derivation from prompt/stop counters (#1170)", () => {
  // `turnActive` derives from `pendingUserPromptSeq > lastStoppedSeq`.
  // The boolean field is kept on `AcpState` as a memoised alias so
  // existing `state.turnActive` reads stay correct, but the counters
  // are the source of truth a late `Stopped` cannot clobber.

  it("isTurnActive flips on / off when counters cross", () => {
    expect(isTurnActive({ pendingUserPromptSeq: 2, lastStoppedSeq: 1 })).toBe(true);
    expect(isTurnActive({ pendingUserPromptSeq: 1, lastStoppedSeq: 1 })).toBe(false);
    expect(isTurnActive({ pendingUserPromptSeq: 0, lastStoppedSeq: 0 })).toBe(false);
  });

  it("Stopped advances lastStoppedSeq by one and recomputes turnActive", () => {
    // Single-prompt happy path: send → Stopped flips turnActive off.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi" } },
    });
    expect(state.pendingUserPromptSeq).toBe(1);
    expect(state.lastStoppedSeq).toBe(0);
    expect(state.turnActive).toBe(true);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.pendingUserPromptSeq).toBe(1);
    expect(state.lastStoppedSeq).toBe(1);
    expect(state.turnActive).toBe(false);
  });

  it("late Stopped from prior turn does NOT clobber turnActive after a fresh follow-up", async () => {
    // The bug. Prior turn: pendingUserPromptSeq=1, lastStoppedSeq=0
    // (turnActive=true). User submits a follow-up before the prior
    // turn's Stopped frame has been applied client-side; the
    // optimistic `user_prompt` action bumps pending to 2. A beat
    // later the Stopped frame for turn 1 lands. Under the old
    // unconditional `turnActive=false`, the spinner died and the
    // late agent chunks reordered visually below the new prompt.
    // Under the counter model, lastStoppedSeq advances to 1
    // (capped at pending) and `2 > 1` keeps turnActive true.
    const { acpHookReducer } = await import("../hooks/useAcpSession");

    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "first turn" } },
    });
    expect(state.turnActive).toBe(true);
    // User taps Send the instant the turn ends; the optimistic
    // dispatch lands BEFORE the Stopped frame for the prior turn.
    state = acpHookReducer(state, {
      kind: "user_prompt",
      text: "follow-up",
    });
    expect(state.pendingUserPromptSeq).toBe(2);
    expect(state.turnActive).toBe(true);
    // Late Stopped (was for turn 1) now arrives. Must NOT kill the
    // spinner because turn 2 is the active turn.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.pendingUserPromptSeq).toBe(2);
    expect(state.lastStoppedSeq).toBe(1);
    expect(state.turnActive).toBe(true);

    // Eventually turn 2's own Stopped lands and flips it off.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.lastStoppedSeq).toBe(2);
    expect(state.turnActive).toBe(false);
  });

  it("spurious Stopped on an idle session does not flip a future prompt off", () => {
    // Defence-in-depth: a Stopped frame arriving with no outstanding
    // turn must not advance `lastStoppedSeq` past `pendingUserPromptSeq`,
    // otherwise the next prompt's increment wouldn't catch up and
    // `turnActive` would stay false even with a real turn in flight.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.turnActive).toBe(false);
    // Spurious extra Stopped (e.g. duplicate replay of the close).
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.lastStoppedSeq).toBe(1);
    expect(state.pendingUserPromptSeq).toBe(1);
    // Next real prompt: turn must reactivate.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 4,
      event: { UserPromptSent: { text: "second" } },
    });
    expect(state.pendingUserPromptSeq).toBe(2);
    expect(state.lastStoppedSeq).toBe(1);
    expect(state.turnActive).toBe(true);
  });

  it("optimistic user_prompt + matching server echo (by prompt_id) only bump pending once", async () => {
    // Avoids double-counting: the server's UserPromptSent whose prompt_id
    // matches an outstanding optimistic overlay row must not bump
    // `pendingUserPromptSeq` again. See #1170 / #3173.
    const { acpHookReducer } = await import("../hooks/useAcpSession");
    let state = acpHookReducer(emptyAcpState(), {
      kind: "user_prompt",
      id: "cmp-echo",
      text: "echo me",
    });
    expect(state.pendingUserPromptSeq).toBe(1);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 5,
      event: { UserPromptSent: { text: "echo me", prompt_id: "cmp-echo" } },
    });
    expect(state.pendingUserPromptSeq).toBe(1);
    expect(state.turnActive).toBe(true);
  });

  it("AgentStartupError advances lastStoppedSeq, preserving the race-safe semantics", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "first" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AgentStartupError: { message: "boom" } },
    });
    expect(state.lastStoppedSeq).toBe(1);
    expect(state.turnActive).toBe(false);
    expect(state.startupError).toBe("boom");
  });

  it("optimistic-match UserPromptSent (by prompt_id) resets per-turn flags without double-counting", () => {
    // A server echo whose prompt_id matches the outstanding optimistic overlay
    // still applies the per-turn resets (worker banners, wakeup countdown) but
    // must NOT bump the counter again. See #1170 / #3173.
    const stale: AcpState = {
      ...withOptimisticPrompt(emptyAcpState(), "follow-up", "cmp-fu"),
      workerStopped: true,
      workerRestarting: true,
      nextWakeupAt: new Date(Date.now() - 1_000).toISOString(),
      nextWakeupReason: "tick",
    };
    const next = applyEvent(stale, {
      session_id: "s-1",
      seq: 9,
      event: { UserPromptSent: { text: "follow-up", prompt_id: "cmp-fu" } },
    });
    expect(next.workerStopped).toBe(false);
    expect(next.workerRestarting).toBe(false);
    expect(next.nextWakeupAt).toBeNull();
    expect(next.nextWakeupReason).toBeNull();
    // withOptimisticPrompt bumped it to 1; the matching echo keeps it at 1.
    expect(next.pendingUserPromptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
  });
});

describe("normaliseTurnCounters (#1170 persisted-state backfill)", () => {
  it("backfills counters from cached turnActive=true", () => {
    const cached = {
      ...emptyAcpState(),
      turnActive: true,
    } as AcpState & { pendingUserPromptSeq?: number; lastStoppedSeq?: number };
    delete cached.pendingUserPromptSeq;
    delete cached.lastStoppedSeq;
    const normalised = normaliseTurnCounters(cached);
    expect(normalised.pendingUserPromptSeq).toBe(1);
    expect(normalised.lastStoppedSeq).toBe(0);
    expect(normalised.turnActive).toBe(true);
  });

  it("backfills counters from cached turnActive=false", () => {
    const cached = {
      ...emptyAcpState(),
      turnActive: false,
    } as AcpState & { pendingUserPromptSeq?: number; lastStoppedSeq?: number };
    delete cached.pendingUserPromptSeq;
    delete cached.lastStoppedSeq;
    const normalised = normaliseTurnCounters(cached);
    expect(normalised.pendingUserPromptSeq).toBe(0);
    expect(normalised.lastStoppedSeq).toBe(0);
    expect(normalised.turnActive).toBe(false);
  });

  it("passes through entries that already carry counters", () => {
    const fresh: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 5,
      lastStoppedSeq: 3,
      turnActive: false,
    };
    const normalised = normaliseTurnCounters(fresh);
    expect(normalised.pendingUserPromptSeq).toBe(5);
    expect(normalised.lastStoppedSeq).toBe(3);
    // Even if the cached `turnActive` boolean was stale, the derived
    // value wins so the spinner gate matches the counters.
    expect(normalised.turnActive).toBe(true);
  });
});

describe("compaction reminder dismissal", () => {
  const usageFrame = (seq: number, used: number, size = 200_000): AcpFrame => ({
    session_id: "s-1",
    seq,
    event: { UsageUpdated: { usage: { used, size } } },
  });

  it("survives usage climbing further, and re-arms after a context boundary", async () => {
    const { acpHookReducer } = await import("../hooks/useAcpSession");
    let state = applyEvent(emptyAcpState(), usageFrame(1, 160_000));

    state = acpHookReducer(state, { kind: "dismiss_compaction_reminder" });
    expect(state.compactionReminderDismissed?.used).toBe(160_000);

    // Still dismissed as the window keeps filling: dismiss means dismiss,
    // not snooze until the next percentage point.
    state = applyEvent(state, usageFrame(2, 180_000));
    expect(state.compactionReminderDismissed?.used).toBe(160_000);

    // Compaction nulls the snapshot, so the next one is a fresh window and
    // re-arms the reminder.
    state = applyEvent(state, { session_id: "s-1", seq: 3, event: "ConversationCompacted" });
    expect(state.sessionUsage).toBeNull();
    state = applyEvent(state, usageFrame(4, 20_000));
    expect(state.compactionReminderDismissed).toBeNull();

    // The regression this shape exists for: re-arming has to latch at the
    // boundary. Deriving "used dropped below the dismissed value" at render
    // time re-suppresses the reminder the moment usage climbs back past it.
    state = acpHookReducer(state, { kind: "dismiss_compaction_reminder" });
    state = applyEvent(state, usageFrame(5, 30_000));
    expect(state.compactionReminderDismissed?.used).toBe(20_000);
    state = applyEvent(state, { session_id: "s-1", seq: 6, event: "SessionCleared" });
    state = applyEvent(state, usageFrame(7, 40_000));
    expect(state.compactionReminderDismissed).toBeNull();
  });

  it("re-arms on every boundary that nulls the usage snapshot", async () => {
    const { acpHookReducer } = await import("../hooks/useAcpSession");
    const boundaries: AcpFrame["event"][] = [
      "ConversationCompacted",
      "SessionCleared",
      { SessionContextReset: { reason: "session/load failed: bad id" } },
      { AgentSwitched: { from: "claude", to: "codex", reason: "rate_limit" } },
    ];
    for (const event of boundaries) {
      let state = applyEvent(emptyAcpState(), usageFrame(1, 160_000));
      state = acpHookReducer(state, { kind: "dismiss_compaction_reminder" });
      state = applyEvent(state, { session_id: "s-1", seq: 2, event });
      state = applyEvent(state, usageFrame(3, 170_000));
      expect(state.compactionReminderDismissed, JSON.stringify(event)).toBeNull();
    }
  });

  it("backfills the dismissal on entries persisted before it existed", () => {
    const persisted = { ...emptyAcpState() } as AcpState & {
      compactionReminderDismissed?: AcpState["compactionReminderDismissed"];
    };
    delete persisted.compactionReminderDismissed;
    expect(normaliseTurnCounters(persisted).compactionReminderDismissed).toBeNull();
  });
});

describe("acpHookReducer / dismiss_primer", () => {
  // Banner dismiss used to live in component-local useState and
  // re-armed itself on every session switch. Moved into the reducer so
  // the dismissal survives mount/unmount; the next SessionContextReset
  // re-seeds contextPrimerAvailable with a new resetSeq so a later
  // incident still surfaces the banner. See #1110.
  it("clears contextPrimerAvailable", async () => {
    const { acpHookReducer } = await import("../hooks/useAcpSession");
    const seeded: AcpState = {
      ...emptyAcpState(),
      contextPrimerAvailable: {
        resetSeq: 12,
        reason: "Conversation context reset; agent transcript was unavailable.",
      },
    };
    const next = acpHookReducer(seeded, { kind: "dismiss_primer" });
    expect(next.contextPrimerAvailable).toBeNull();
  });
});

describe("applyEvent / ModeSwitchFailed", () => {
  it("captures the rejected mode + reason", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ModeSwitchFailed: {
          mode_id: "bypassPermissions",
          reason: "Mode bypassPermissions is not available.",
        },
      },
    });
    expect(next.modeSwitchFailed).not.toBeNull();
    expect(next.modeSwitchFailed?.modeId).toBe("bypassPermissions");
    expect(next.modeSwitchFailed?.reason).toBe("Mode bypassPermissions is not available.");
  });

  it("clears when a subsequent CurrentModeChanged lands", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ModeSwitchFailed: { mode_id: "bypassPermissions", reason: "denied" },
      },
    });
    expect(state.modeSwitchFailed).not.toBeNull();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { CurrentModeChanged: { current_mode_id: "acceptEdits" } },
    });
    expect(state.modeSwitchFailed).toBeNull();
    expect(state.currentModeId).toBe("acceptEdits");
  });
});

describe("acpHookReducer / dismiss_mode_switch_failed", () => {
  it("clears the notice", async () => {
    const { acpHookReducer } = await import("../hooks/useAcpSession");
    const seeded: AcpState = {
      ...emptyAcpState(),
      modeSwitchFailed: {
        modeId: "bypassPermissions",
        reason: "denied",
        at: new Date().toISOString(),
      },
    };
    const next = acpHookReducer(seeded, {
      kind: "dismiss_mode_switch_failed",
    });
    expect(next.modeSwitchFailed).toBeNull();
  });
});

// Reducer coverage for the silent-orphan watchdog (#1240). The
// daemon-side detector is exercised by the Rust integration test in
// tests/acp_silent_orphan.rs; this block just pins down the
// frontend half so a future refactor of the worker-state banner
// doesn't silently regress the prompt_orphaned path.
function stoppedFrame(reason: string, seq: number): AcpFrame {
  return {
    session_id: "s-orphan",
    seq,
    event: { Stopped: { reason } },
  };
}

describe("AcpState reducer / silent-orphan watchdog (#1240)", () => {
  it("sets agentOrphaned and workerRestarting on prompt_orphaned", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 1,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 1));
    expect(state.agentOrphaned).toBe(true);
    expect(state.workerRestarting).toBe(true);
    expect(state.workerStopped).toBe(false);
    expect(state.agentUnresponsive).toBe(false);
  });

  it("clears agentUnresponsive when prompt_orphaned arrives after it", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 2,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("agent_unresponsive", 1));
    expect(state.agentUnresponsive).toBe(true);
    expect(state.agentOrphaned).toBe(false);
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 2));
    expect(state.agentUnresponsive).toBe(false);
    expect(state.agentOrphaned).toBe(true);
  });

  it("clears agentOrphaned on AcpSessionAssigned (respawn completed)", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 1,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 1));
    expect(state.agentOrphaned).toBe(true);
    state = applyEvent(state, {
      session_id: "s-orphan",
      seq: 2,
      event: { AcpSessionAssigned: { acp_session_id: "sess-abc" } },
    });
    expect(state.agentOrphaned).toBe(false);
    expect(state.workerRestarting).toBe(false);
  });

  it("clears agentOrphaned on UserPromptSent (user moving on)", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 1,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 1));
    expect(state.agentOrphaned).toBe(true);
    state = applyEvent(state, {
      session_id: "s-orphan",
      seq: 2,
      event: { UserPromptSent: { text: "next prompt" } },
    });
    expect(state.agentOrphaned).toBe(false);
  });

  it("clears agentOrphaned on user_stopped", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 1,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 1));
    expect(state.agentOrphaned).toBe(true);
    state = applyEvent(state, stoppedFrame("user_stopped", 2));
    expect(state.agentOrphaned).toBe(false);
  });

  it("backfills agentOrphaned=false on pre-#1240 persisted state", () => {
    // Simulate a localStorage entry written before #1240: agentOrphaned
    // absent. normaliseTurnCounters must default it to false so the
    // reducer and banner code see a well-typed value.
    const stale = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 0,
      lastStoppedSeq: 0,
    } as AcpState & { agentOrphaned?: boolean };
    delete stale.agentOrphaned;
    const normalised = normaliseTurnCounters(stale);
    expect(normalised.agentOrphaned).toBe(false);
  });

  it("backfills usageBaseline=null on pre-#1354 persisted state", () => {
    // Simulate a localStorage entry written before #1354: usageBaseline
    // absent. normaliseTurnCounters must default it to null so the
    // UsageUpdated reducer arm's `next.usageBaseline && ...` check sees
    // a well-typed value rather than `undefined`.
    const stale = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 0,
      lastStoppedSeq: 0,
    } as AcpState & { usageBaseline?: { cost: number } | null };
    delete stale.usageBaseline;
    const normalised = normaliseTurnCounters(stale);
    expect(normalised.usageBaseline).toBeNull();
  });

  it("preserves a non-null usageBaseline through normaliseTurnCounters", () => {
    // A session that ran /clear before reload writes a baseline into
    // localStorage. Hydration must keep it so post-reload UsageUpdate
    // frames continue subtracting the boundary cumulative.
    const cached: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 3,
      lastStoppedSeq: 3,
      usageBaseline: { cost: 0.42 },
    };
    const normalised = normaliseTurnCounters(cached);
    expect(normalised.usageBaseline?.cost).toBeCloseTo(0.42, 6);
  });

  it("clears agentOrphaned on restart_pending", () => {
    // Supervisor's reap_user_stopped sweep publishes restart_pending
    // when a worker disappears out-of-band; that supersedes a prior
    // orphan escalation, so the banner must downgrade to the generic
    // "Restarting…" copy. See CodeRabbit review on #1248.
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 1,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 1));
    expect(state.agentOrphaned).toBe(true);
    state = applyEvent(state, stoppedFrame("restart_pending", 2));
    expect(state.agentOrphaned).toBe(false);
    expect(state.workerRestarting).toBe(true);
  });

  it("clears agentOrphaned when agent_unresponsive arrives next", () => {
    // The cancel-escalation watchdog (agent_unresponsive) is the
    // proximate path that downstream supervisor logic uses to drive
    // SIGTERM + respawn even when the silent-orphan watchdog (#1240)
    // armed first. If both reasons fire in sequence, the banner must
    // flip away from agentOrphaned so the user sees the cancel-
    // escalation copy that matches the active recovery phase.
    let state: AcpState = {
      ...emptyAcpState(),
      pendingUserPromptSeq: 2,
      lastStoppedSeq: 0,
    };
    state = applyEvent(state, stoppedFrame("prompt_orphaned", 1));
    expect(state.agentOrphaned).toBe(true);
    state = applyEvent(state, stoppedFrame("agent_unresponsive", 2));
    expect(state.agentOrphaned).toBe(false);
    expect(state.agentUnresponsive).toBe(true);
    expect(state.workerRestarting).toBe(true);
  });
});

describe("applyEvent / IncompatibleAgent (claude-agent-acp v0.39.0)", () => {
  it("sets state.incompatibleAgent from the structured detail", () => {
    const next = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        IncompatibleAgent: {
          detail: {
            kind: "incompatible_agent_version",
            package_name: "@agentclientprotocol/claude-agent-acp",
            installed: "0.32.0",
            required: "0.39.0",
            install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
          },
        },
      },
    });
    expect(next.incompatibleAgent).not.toBeNull();
    expect(next.incompatibleAgent?.kind).toBe("incompatible_agent_version");
    if (next.incompatibleAgent?.kind === "incompatible_agent_version") {
      expect(next.incompatibleAgent.installed).toBe("0.32.0");
      expect(next.incompatibleAgent.required).toBe("0.39.0");
    }
  });

  it("clears incompatibleAgent on AcpSessionAssigned (respawn healed)", () => {
    let state: AcpState = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        IncompatibleAgent: {
          detail: {
            kind: "incompatible_agent_version",
            package_name: "@agentclientprotocol/claude-agent-acp",
            installed: "0.32.0",
            required: "0.39.0",
            install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
          },
        },
      },
    });
    expect(state.incompatibleAgent).not.toBeNull();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AcpSessionAssigned: { acp_session_id: "acp-1" } },
    });
    expect(state.incompatibleAgent).toBeNull();
  });
});

describe("applyEvent / ConfigOptions (#1403)", () => {
  function sampleOptions() {
    return [
      {
        id: "model",
        name: "Model",
        category: "model" as const,
        current_value: "claude-opus-4-7",
        options: [
          { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
          { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
        ],
      },
      {
        id: "effort",
        name: "Reasoning Effort",
        category: "thought_level" as const,
        current_value: "default",
        options: [
          { value: "default", name: "Default" },
          { value: "high", name: "High" },
        ],
      },
    ];
  }

  it("applies ConfigOptionsUpdated as a full snapshot replacement", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ConfigOptionsUpdated: { options: sampleOptions() } },
    });
    expect(state.configOptions).toHaveLength(2);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ConfigOptionsUpdated: {
          options: [
            {
              id: "model",
              name: "Model",
              category: "model",
              current_value: "claude-sonnet-4-6",
              options: [],
            },
          ],
        },
      },
    });
    expect(state.configOptions).toHaveLength(1);
    expect(state.configOptions[0].current_value).toBe("claude-sonnet-4-6");
  });

  it("populates configOptionSwitchFailed without mutating configOptions", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ConfigOptionsUpdated: { options: sampleOptions() } },
    });
    const before = state.configOptions;
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ConfigOptionSwitchFailed: {
          config_id: "model",
          value: "claude-sonnet-4-6",
          reason: "rate limited",
        },
      },
    });
    expect(state.configOptions).toBe(before);
    expect(state.configOptionSwitchFailed).toEqual({
      configId: "model",
      value: "claude-sonnet-4-6",
      reason: "rate limited",
      at: expect.any(String),
    });
  });

  it("clears pending and auto-dismisses matching failure on confirming snapshot", () => {
    let state: AcpState = {
      ...emptyAcpState(),
      pendingConfigOption: { configId: "model", value: "claude-sonnet-4-6" },
    };
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 1,
      event: {
        ConfigOptionSwitchFailed: {
          config_id: "model",
          value: "claude-sonnet-4-6",
          reason: "transient",
        },
      },
    });
    expect(state.pendingConfigOption).toBeNull();
    expect(state.configOptionSwitchFailed).not.toBeNull();

    const confirming = sampleOptions();
    confirming[0].current_value = "claude-sonnet-4-6";
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ConfigOptionsUpdated: { options: confirming } },
    });
    expect(state.configOptionSwitchFailed).toBeNull();
    expect(state.pendingConfigOption).toBeNull();
  });

  it("preserves a non-matching failure notice across snapshots", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ConfigOptionsUpdated: { options: sampleOptions() } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ConfigOptionSwitchFailed: {
          config_id: "model",
          value: "claude-sonnet-4-6",
          reason: "transient",
        },
      },
    });
    // Snapshot still shows opus as current; the failure for the sonnet
    // switch attempt must survive.
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { ConfigOptionsUpdated: { options: sampleOptions() } },
    });
    expect(state.configOptionSwitchFailed).not.toBeNull();
  });

  it("AgentSwitched clears configOptions and the failure notice", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ConfigOptionsUpdated: { options: sampleOptions() } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ConfigOptionSwitchFailed: {
          config_id: "effort",
          value: "high",
          reason: "unsupported",
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        AgentSwitched: { from: "claude", to: "codex", reason: "rate_limit" },
      },
    });
    expect(state.configOptions).toEqual([]);
    expect(state.configOptionSwitchFailed).toBeNull();
    expect(state.pendingConfigOption).toBeNull();
  });

  it("SessionCleared preserves configOptions (adapter capabilities outlive /clear)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ConfigOptionsUpdated: { options: sampleOptions() } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.configOptions).toHaveLength(2);
  });
});

describe("applyEvent / thinking-state honesty (#1213)", () => {
  // claude-agent-acp emits ThinkingStarted once per reasoning block but
  // often skips ThinkingEnded when it transitions into tool calls or
  // final text. Without these clears, `thinking` latches true through a
  // whole turn and the WorkingSpinner shows "thinking" verbs while a
  // Terminal command is actually running. See #1213.

  function toolCall(id: string, name: string): ToolCall {
    return {
      id,
      name,
      kind: "execute",
      args_preview: "{}",
      started_at: "2026-01-01T00:00:00Z",
    };
  }

  it("clears thinking when a tool call starts (no ThinkingEnded from adapter)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "ThinkingStarted",
    });
    expect(state.thinking).toBe(true);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ToolCallStarted: { tool_call: toolCall("t1", "Terminal") } },
    });
    expect(state.thinking).toBe(false);
    expect(state.inFlightTool?.name).toBe("Terminal");
  });

  it("clears thinking when assistant text starts streaming", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "ThinkingStarted",
    });
    expect(state.thinking).toBe(true);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AgentMessageChunk: { text: "Here is the answer" } },
    });
    expect(state.thinking).toBe(false);
  });

  it("clears thinking on Stopped so it does not leak across turns", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "ThinkingStarted",
    });
    expect(state.thinking).toBe(true);

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: { reason: "prompt_complete" } },
    });
    expect(state.thinking).toBe(false);
    expect(state.inFlightTool).toBeNull();
  });

  it("derives tool over thinking through an interleaved turn (full trace)", () => {
    // Mirrors the affected session: ThinkingStarted, then a Terminal
    // tool call with no intervening ThinkingEnded.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: "ThinkingStarted",
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ToolCallStarted: { tool_call: toolCall("t1", "Terminal") } },
    });
    // The WorkingSpinner derives state as tool > thinking > working.
    expect(state.thinking).toBe(false);
    expect(state.inFlightTool).not.toBeNull();
  });
});

describe("applyEvent / RateLimitAutoResumed (#1722)", () => {
  it("clears the rate-limit banner so the composer unlocks", () => {
    let state: AcpState = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        RateLimit: {
          info: {
            status: "usage limit reached",
            resets_at: "2026-06-01T12:10:00Z",
            kind: "rate_limit",
          },
        },
      },
    });
    expect(state.rateLimit).not.toBeNull();

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { RateLimitAutoResumed: { resets_at: "2026-06-01T12:10:00Z" } },
    });
    expect(state.rateLimit).toBeNull();
  });
});

describe("applyEvent / elicitation (control state)", () => {
  const elicitation = {
    nonce: "e-1",
    message: "Pick one",
    tool_call_id: null,
    questions: [],
    requested_at: "2026-06-10T00:00:00Z",
    resolved: null,
  };

  it("adds a pending elicitation on ElicitationRequested and drops it on resolve", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { ElicitationRequested: { elicitation } },
    });
    expect(state.pendingElicitations).toHaveLength(1);
    expect(state.pendingElicitations[0].nonce).toBe("e-1");

    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ElicitationResolved: { nonce: "e-1", outcome: "Accepted" } },
    });
    expect(state.pendingElicitations).toHaveLength(0);
  });

  it("clears the in-flight tool pointer when the elicitation names the started tool", () => {
    // The AskUserQuestion tool card suppression is server-owned (Tier 4); the
    // client only drops the in-flight spinner pointer so it doesn't linger on
    // the suppressed call.
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallStarted: {
          tool_call: {
            id: "tc-ask",
            name: "Asking for your input",
            kind: "other",
            args_preview: "{}",
            started_at: "2026-06-10T00:00:00Z",
          },
        },
      },
    });
    expect(state.inFlightTool?.id).toBe("tc-ask");
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { ElicitationRequested: { elicitation: { ...elicitation, tool_call_id: "tc-ask" } } },
    });
    expect(state.inFlightTool).toBeNull();
  });
});

describe("applyEvent / UsageUpdated context-window latch (upstream #596 bandaid)", () => {
  function usageFrame(seq: number, used: number, size: number): AcpFrame {
    return {
      session_id: "s-1",
      seq,
      event: { UsageUpdated: { usage: { used, size } } },
    };
  }
  function modelFrame(seq: number, currentValue: string): AcpFrame {
    return {
      session_id: "s-1",
      seq,
      event: {
        ConfigOptionsUpdated: {
          options: [
            {
              id: "model",
              name: "Model",
              category: "model",
              current_value: currentValue,
              options: [],
            },
          ],
        },
      },
    };
  }

  it("latches the largest window and ignores the mid-turn 200k downgrade", () => {
    let state = applyEvent(emptyAcpState(), usageFrame(1, 10_000, 200_000));
    expect(state.sessionUsage?.size).toBe(200_000);
    // authoritative 1M arrives at turn end
    state = applyEvent(state, usageFrame(2, 20_000, 1_000_000));
    expect(state.sessionUsage?.size).toBe(1_000_000);
    // next turn's mid-stream frame downgrades to 200k; latch holds 1M,
    // but `used` still tracks the incoming value
    state = applyEvent(state, usageFrame(3, 30_000, 200_000));
    expect(state.sessionUsage?.size).toBe(1_000_000);
    expect(state.sessionUsage?.used).toBe(30_000);
  });

  it("resets the latch on a context boundary (SessionCleared)", () => {
    let state = applyEvent(emptyAcpState(), usageFrame(1, 20_000, 1_000_000));
    expect(state.sessionUsage?.size).toBe(1_000_000);
    state = applyEvent(state, { session_id: "s-1", seq: 2, event: "SessionCleared" });
    expect(state.sessionUsage).toBeNull();
    state = applyEvent(state, usageFrame(3, 5_000, 200_000));
    expect(state.sessionUsage?.size).toBe(200_000);
  });

  it("resets the latch when the model changes", () => {
    let state = applyEvent(emptyAcpState(), modelFrame(1, "sonnet"));
    state = applyEvent(state, usageFrame(2, 20_000, 1_000_000));
    expect(state.sessionUsage?.size).toBe(1_000_000);
    // switch to a smaller-window model; the prior 1M latch must not stick
    state = applyEvent(state, modelFrame(3, "haiku"));
    expect(state.sessionUsage).toBeNull();
    state = applyEvent(state, usageFrame(4, 5_000, 200_000));
    expect(state.sessionUsage?.size).toBe(200_000);
  });
});

describe("applyEvent / rate-limit banner clears on resume-to-life", () => {
  const info = {
    status: "rate_limited",
    resets_at: "2026-07-23T15:40:00Z",
    kind: "rate_limit",
  };
  const rateLimitFrame = (seq: number): AcpFrame => ({
    session_id: "s-1",
    seq,
    event: { RateLimit: { info } },
  });

  it("clears rateLimit on the next UserPromptSent (resumed via a plain prompt)", () => {
    // Reproduces #3028: a rate-limited turn parks the banner, the session
    // resumes via a prompt (or a draining queued follow-up), yet the
    // banner never went away because UserPromptSent didn't clear it.
    let state = applyEvent(emptyAcpState(), rateLimitFrame(1));
    expect(state.rateLimit).toEqual(info);
    state = applyEvent(state, frame(2, "continue"));
    expect(state.rateLimit).toBeNull();
  });

  it("clears rateLimit on a fresh AcpSessionAssigned (a new worker healed the park)", () => {
    let state = applyEvent(emptyAcpState(), rateLimitFrame(1));
    expect(state.rateLimit).toEqual(info);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AcpSessionAssigned: { acp_session_id: "acp-1" } },
    });
    expect(state.rateLimit).toBeNull();
  });

  it("re-derives rateLimit === null on replay when a turn resumed after the park", () => {
    // The "stuck 4h later" symptom is replay: reconnect reapplies the
    // event log, so the post-park UserPromptSent must clear the banner
    // every time the state is rebuilt, not just on the live dispatch.
    const log: AcpFrame[] = [rateLimitFrame(1), frame(2, "continue")];
    const replayed = log.reduce(applyEvent, emptyAcpState());
    expect(replayed.rateLimit).toBeNull();
  });
});
