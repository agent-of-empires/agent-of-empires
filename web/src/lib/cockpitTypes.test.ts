// Reducer tests for the cockpit memory/recall feature.
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
  emptyCockpitState,
  isTurnActive,
  normaliseTurnCounters,
  type CockpitFrame,
  type CockpitState,
  type ToolCall,
} from "./cockpitTypes";

function frame(seq: number, text: string): CockpitFrame {
  return {
    session_id: "s-1",
    seq,
    event: { UserPromptSent: { text } },
  };
}

function withOptimisticPrompt(state: CockpitState, text: string): CockpitState {
  // Mirrors the optimistic dispatch in useCockpit.sendPrompt: row id
  // includes the wall-clock timestamp (distinct from the `user-seq-N`
  // form the reducer assigns when the server echoes), and
  // `pendingUserPromptSeq` bumps so a subsequent server echo on the
  // matching row doesn't double-count. See #1170.
  const pendingUserPromptSeq = state.pendingUserPromptSeq + 1;
  return {
    ...state,
    activity: state.activity.concat({
      id: `user-${Date.now()}-${state.activity.length}`,
      kind: "user_prompt",
      text,
      at: new Date().toISOString(),
    }),
    pendingUserPromptSeq,
    turnActive: pendingUserPromptSeq > state.lastStoppedSeq,
  };
}

describe("applyEvent / UserPromptSent", () => {
  it("appends a user_prompt row when no optimistic placeholder exists", () => {
    const next = applyEvent(emptyCockpitState(), frame(1, "hi"));
    expect(next.activity).toHaveLength(1);
    expect(next.activity[0]).toMatchObject({
      id: "user-seq-1",
      kind: "user_prompt",
      text: "hi",
    });
    expect(next.lastSeq).toBe(1);
    expect(next.turnActive).toBe(true);
  });

  it("dedupes against the optimistic row by promoting its id", () => {
    // Simulate: useCockpit.sendPrompt fires an optimistic dispatch,
    // then the server's UserPromptSent echo arrives over the WS.
    const optimistic = withOptimisticPrompt(emptyCockpitState(), "test prompt");
    expect(optimistic.activity).toHaveLength(1);
    expect(optimistic.activity[0].id.startsWith("user-seq-")).toBe(false);

    const next = applyEvent(optimistic, frame(7, "test prompt"));
    // Single row preserved, id rewritten to the authoritative form so
    // future replays dedupe against it via seq.
    expect(next.activity).toHaveLength(1);
    expect(next.activity[0].id).toBe("user-seq-7");
    expect(next.activity[0].text).toBe("test prompt");
    expect(next.lastSeq).toBe(7);
  });

  it("does not dedupe when the optimistic text differs from the echo", () => {
    // Edge case: user typed two prompts back-to-back. The optimistic
    // row for the FIRST prompt should not be overwritten by the
    // server echo of the SECOND prompt.
    const optimistic = withOptimisticPrompt(emptyCockpitState(), "first");
    const next = applyEvent(optimistic, frame(2, "second"));
    expect(next.activity).toHaveLength(2);
    expect(next.activity[0].text).toBe("first");
    expect(next.activity[1].id).toBe("user-seq-2");
    expect(next.activity[1].text).toBe("second");
  });

  it("dedupes the OLDEST matching optimistic row when same text is sent twice", () => {
    // Regression: user clicks Send with the same text twice in quick
    // succession. Two optimistic rows are queued. The first server
    // echo (seq=N) corresponds to the first submission and must
    // promote row 0, not row 1. If we promoted the most-recent row,
    // row 0 would be orphaned forever and the second echo (seq=N+1)
    // would append a third row, leaving the user with three rows on
    // screen for two prompts.
    let state = withOptimisticPrompt(emptyCockpitState(), "ping");
    state = withOptimisticPrompt(state, "ping");
    expect(state.activity).toHaveLength(2);

    state = applyEvent(state, frame(10, "ping"));
    state = applyEvent(state, frame(11, "ping"));

    expect(state.activity).toHaveLength(2);
    expect(state.activity[0].id).toBe("user-seq-10");
    expect(state.activity[1].id).toBe("user-seq-11");
    expect(state.activity[0].text).toBe("ping");
    expect(state.activity[1].text).toBe("ping");
  });

  it("does not double-dedupe a prompt that already has a seq-based id", () => {
    // Replay scenario: reducer applied frame(seq=3) once, then a
    // later reconnect re-delivers the same frame. Without seq dedupe
    // the reducer would walk the optimistic-promotion branch a second
    // time and clobber the row's metadata.
    let state = applyEvent(emptyCockpitState(), frame(3, "echoed"));
    expect(state.activity[0].id).toBe("user-seq-3");

    // Re-deliver the same frame — frame.seq <= state.lastSeq must be
    // a no-op so the same row isn't promoted again.
    state = applyEvent(state, frame(3, "echoed"));
    expect(state.activity).toHaveLength(1);
    expect(state.activity[0].id).toBe("user-seq-3");
    expect(state.lastSeq).toBe(3);
  });

  it("clears assistantMessage and turnActive flags so the new turn starts clean", () => {
    const stale: CockpitState = {
      ...emptyCockpitState(),
      assistantMessage: "stale partial reply",
      startupError: "old error",
      lastError: "old action error",
      turnActive: false,
    };
    const next = applyEvent(stale, frame(1, "new prompt"));
    expect(next.assistantMessage).toBe("");
    expect(next.startupError).toBeNull();
    expect(next.lastError).toBeNull();
    expect(next.turnActive).toBe(true);
  });

  it("renders tool output from ToolCallCompleted.content", () => {
    // Most agents (Claude's claude-agent-acp included) ship the tool's
    // textual output on the *completion* update via fields.content. If
    // we lose this, the bash card body literally reads "completed".
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallStarted: {
          tool_call: {
            id: "tc-bash",
            name: "Terminal",
            kind: "execute",
            args_preview: "{}",
            started_at: new Date().toISOString(),
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ToolCallCompleted: {
          tool_call_id: "tc-bash",
          is_error: false,
          content: "abc1234 first commit\ndef5678 second commit\n",
        },
      },
    });
    const done = state.activity.find((a) => a.id === "done-tc-bash");
    expect(done).toBeDefined();
    expect(done!.kind).toBe("tool_complete");
    expect(done!.text).toBe(
      "abc1234 first commit\ndef5678 second commit\n",
    );
    expect(state.inFlightTool).toBeNull();
  });

  it("falls back to streamed ToolCallContent when completion has empty content", () => {
    // Some agents stream stdout via interim ToolCallUpdate notifications
    // (status=in_progress with content) and emit a final completion
    // with empty content. The reducer buffers interim chunks keyed by
    // tool_call_id and drains the buffer on completion.
    let state = emptyCockpitState();
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallContent: {
          tool_call_id: "tc-bash",
          content: "line1\n",
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ToolCallContent: {
          tool_call_id: "tc-bash",
          content: "line1\nline2\n",
        },
      },
    });
    expect(state.toolOutputs["tc-bash"]).toBe("line1\nline2\n");
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: {
        ToolCallCompleted: {
          tool_call_id: "tc-bash",
          is_error: false,
          content: "",
        },
      },
    });
    const done = state.activity.find((a) => a.id === "done-tc-bash");
    expect(done!.text).toBe("line1\nline2\n");
    // Buffer drained so a re-completion (replay) doesn't double-render.
    expect(state.toolOutputs["tc-bash"]).toBeUndefined();
  });

  it("falls back to status word when no content arrived at all", () => {
    const state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallCompleted: {
          tool_call_id: "tc-x",
          is_error: false,
          content: "",
        },
      },
    });
    const done = state.activity.find((a) => a.id === "done-tc-x");
    expect(done!.text).toBe("completed");
  });

  it("patches tool_start args/title when ToolCallUpdated arrives later", () => {
    // Claude's claude-agent-acp emits the initial tool_call with an
    // empty raw_input and a generic title ("Terminal"); the actual
    // command lands in a follow-up ToolCallUpdate. The reducer must
    // overwrite the row's tool payload so the card header shows
    // `$ git log -n 10` rather than `$ Terminal`.
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallStarted: {
          tool_call: {
            id: "tc-bash",
            name: "Terminal",
            kind: "execute",
            args_preview: "{}",
            started_at: new Date().toISOString(),
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ToolCallUpdated: {
          tool_call_id: "tc-bash",
          title: null,
          args_preview: '{"command":"git log -n 10"}',
        },
      },
    });
    const startRow = state.activity.find(
      (a) => a.kind === "tool_start" && a.toolCallId === "tc-bash",
    );
    expect(startRow?.tool?.args_preview).toBe(
      '{"command":"git log -n 10"}',
    );
    expect(startRow?.tool?.name).toBe("Terminal");
    expect(state.inFlightTool?.args_preview).toBe(
      '{"command":"git log -n 10"}',
    );
  });

  it("uses 'tool failed' when error event has no content", () => {
    const state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        ToolCallCompleted: {
          tool_call_id: "tc-y",
          is_error: true,
          content: "",
        },
      },
    });
    const done = state.activity.find((a) => a.id === "done-tc-y");
    expect(done!.kind).toBe("tool_error");
    expect(done!.text).toBe("tool failed");
  });

  it("reconstructs the user side of the conversation from a replay", () => {
    // Server restart scenario: client connects, WS drain delivers all
    // events from the on-disk store including UserPromptSent rows.
    // Without these, the assistant chunks would collapse into a
    // single blob; with them, each turn gets its own user message.
    const replay: CockpitFrame[] = [
      { session_id: "s-1", seq: 1, event: { UserPromptSent: { text: "hi" } } },
      {
        session_id: "s-1",
        seq: 2,
        event: { AgentMessageChunk: { text: "Hello!" } },
      },
      {
        session_id: "s-1",
        seq: 3,
        event: { UserPromptSent: { text: "thanks" } },
      },
      {
        session_id: "s-1",
        seq: 4,
        event: { AgentMessageChunk: { text: "Anytime." } },
      },
    ];
    const final = replay.reduce(
      (state, f) => applyEvent(state, f),
      emptyCockpitState(),
    );
    const userPrompts = final.activity.filter((a) => a.kind === "user_prompt");
    const messages = final.activity.filter((a) => a.kind === "message");
    expect(userPrompts.map((u) => u.text)).toEqual(["hi", "thanks"]);
    expect(messages.map((m) => m.text)).toEqual(["Hello!", "Anytime."]);
    expect(final.lastSeq).toBe(4);
  });
});

describe("applyEvent / UserDiffCommentsPrompt (#1123)", () => {
  function diffCommentsFrame(seq: number): CockpitFrame {
    return {
      session_id: "s-1",
      seq,
      event: {
        UserDiffCommentsPrompt: {
          intro: "Take a look:",
          outro: "Please address these comments.",
          isMultiRepo: true,
          comments: [
            {
              id: "c-1",
              repoName: "repoA",
              filePath: "src/main.rs",
              side: "new",
              startLine: 42,
              endLine: 45,
              body: "rename this",
              capturedSnippet: "fn main() {}",
              language: "rust",
              createdAt: "2026-01-01T00:00:00Z",
            },
          ],
          assembledMarkdown: "Take a look:\n\n## Diff comments\n\n...\n",
        },
      },
    };
  }

  it("appends a typed user_diff_comments row carrying the structured payload", () => {
    const next = applyEvent(emptyCockpitState(), diffCommentsFrame(1));
    expect(next.activity).toHaveLength(1);
    const row = next.activity[0]!;
    expect(row.id).toBe("user-seq-1");
    expect(row.kind).toBe("user_diff_comments");
    // text is the assembled markdown (agent-visible body / fallback),
    // never a base64 sentinel.
    expect(row.text).toContain("## Diff comments");
    expect(row.text).not.toContain("aoe:diff-comments");
    expect(row.diffComments).toEqual({
      intro: "Take a look:",
      outro: "Please address these comments.",
      isMultiRepo: true,
      comments: [
        {
          id: "c-1",
          repoName: "repoA",
          filePath: "src/main.rs",
          side: "new",
          startLine: 42,
          endLine: 45,
          body: "rename this",
          capturedSnippet: "fn main() {}",
          language: "rust",
          createdAt: "2026-01-01T00:00:00Z",
        },
      ],
    });
    expect(next.lastSeq).toBe(1);
    expect(next.turnActive).toBe(true);
  });

  it("applies the same per-turn resets as a plain prompt", () => {
    const stale: CockpitState = {
      ...emptyCockpitState(),
      assistantMessage: "stale partial reply",
      startupError: "old error",
      lastError: "old action error",
      workerStopped: true,
      workerRestarting: true,
      agentUnresponsive: true,
      turnActive: false,
    };
    const next = applyEvent(stale, diffCommentsFrame(1));
    expect(next.assistantMessage).toBe("");
    expect(next.startupError).toBeNull();
    expect(next.lastError).toBeNull();
    expect(next.workerStopped).toBe(false);
    expect(next.workerRestarting).toBe(false);
    expect(next.agentUnresponsive).toBe(false);
    expect(next.turnActive).toBe(true);
  });

  it("counts as a prior user turn for SessionContextReset (#1123)", () => {
    // A session whose only turn is a diff-comments prompt must still
    // surface the context-reset row + arm the primer; otherwise it is
    // wrongly treated as a 0-message session.
    let state = applyEvent(emptyCockpitState(), diffCommentsFrame(1));
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "session/load failed: bad id" } },
    });
    expect(state.activity.some((r) => r.kind === "context_reset")).toBe(true);
    expect(state.contextPrimerAvailable).toEqual({
      resetSeq: 2,
      reason: "session/load failed: bad id",
    });
  });

  it("reconstructs the diff-comments turn on replay (no optimistic row)", () => {
    // The send dialog posts directly, so there is never a placeholder to
    // promote; the server echo simply appends the typed row.
    const final = [
      diffCommentsFrame(1),
      {
        session_id: "s-1",
        seq: 2,
        event: { AgentMessageChunk: { text: "On it." } },
      } as CockpitFrame,
    ].reduce((state, f) => applyEvent(state, f), emptyCockpitState());
    const rows = final.activity.filter((a) => a.kind === "user_diff_comments");
    expect(rows).toHaveLength(1);
    expect(rows[0]!.diffComments?.comments).toHaveLength(1);
    expect(final.lastSeq).toBe(2);
  });
});

describe("applyEvent / AvailableCommandsUpdated", () => {
  it("populates availableCommands and replaces the prior list", () => {
    const f1: CockpitFrame = {
      session_id: "s-1",
      seq: 1,
      event: {
        AvailableCommandsUpdated: {
          commands: [
            { name: "help", description: "Show help", accepts_input: false },
          ],
        },
      },
    };
    const s1 = applyEvent(emptyCockpitState(), f1);
    expect(s1.availableCommands).toHaveLength(1);
    expect(s1.availableCommands[0].name).toBe("help");

    const f2: CockpitFrame = {
      session_id: "s-1",
      seq: 2,
      event: {
        AvailableCommandsUpdated: {
          commands: [
            { name: "review", description: "Review PR", accepts_input: true },
            { name: "clear", description: "Clear context", accepts_input: false },
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
    const before = emptyCockpitState();
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

  it("SessionContextReset clears stale usage and appends a context_reset row", () => {
    let state = applyEvent(emptyCockpitState(), {
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
    const last = state.activity[state.activity.length - 1];
    expect(last?.kind).toBe("context_reset");
    expect(last?.text).toContain("session/load failed");
  });

  it("SessionContextReset uses a fallback message when reason is empty", () => {
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { SessionContextReset: { reason: "" } },
    });
    const last = state.activity[state.activity.length - 1];
    expect(last?.kind).toBe("context_reset");
    expect(last?.text.length).toBeGreaterThan(0);
  });

  it("SessionContextReset is silent on a session with no prior user prompt", () => {
    // 0-message session: agent never persisted a transcript, so
    // session/load failing on the next spawn is expected. Don't
    // surface a meaningless "context reset" warning.
    let state = applyEvent(emptyCockpitState(), {
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
    // No visible row appended.
    expect(state.activity.some((r) => r.kind === "context_reset")).toBe(false);
    expect(state.lastSeq).toBe(2);
  });

  it("SessionContextReset that arrives BEFORE the first prompt stays hidden after later prompts", () => {
    // Replay order: reset@2, then prompt@3. The reset must NOT appear
    // above the prompt later — applyEvent processes events in seq order
    // and decides based on what's been seen so far.
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    const state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { SessionContextReset: { reason: "load failed" } },
    });
    expect(state.contextPrimerAvailable).toBeNull();
  });

  it("UserPromptSent clears contextPrimerAvailable (one-shot affordance)", () => {
    let state = applyEvent(emptyCockpitState(), {
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

describe("applyEvent / Stopped empty-output fallback", () => {
  it("appends an empty_output row when the turn ended with no agent output", () => {
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "/usage" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { Stopped: {} },
    });
    const last = state.activity[state.activity.length - 1];
    expect(last?.kind).toBe("empty_output");
    expect(last?.text).toContain("no output");
    expect(state.turnActive).toBe(false);
  });

  it("does not append the notice when the agent emitted a message", () => {
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "/context" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: { AgentMessageChunk: { text: "Context Usage" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { Stopped: {} },
    });
    expect(state.activity.find((r) => r.kind === "empty_output")).toBeUndefined();
  });

  it("does not append the notice when a tool call ran during the turn", () => {
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "do a thing" } },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        ToolCallStarted: {
          tool_call: {
            id: "t1",
            name: "Bash",
            kind: "execute",
            args_preview: "{}",
            started_at: new Date().toISOString(),
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 3,
      event: { Stopped: {} },
    });
    expect(state.activity.find((r) => r.kind === "empty_output")).toBeUndefined();
  });
});

describe("applyEvent / Stopped user_stopped", () => {
  it("sets workerStopped on reason=user_stopped and clears turnActive", () => {
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    const state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { Stopped: { reason: "restart_pending" } },
    });
    expect(state.workerRestarting).toBe(true);
    expect(state.workerStopped).toBe(false);
    expect(state.turnActive).toBe(false);
  });

  it("clears workerRestarting on AcpSessionAssigned (reconciler auto-respawn finished)", () => {
    let state = applyEvent(emptyCockpitState(), {
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
    // Edge case: user runs `aoe cockpit stop`, then realises they meant
    // `restart`. The two reasons must not pile up — restart_pending
    // wins because it's the most recent signal from the daemon.
    let state = applyEvent(emptyCockpitState(), {
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

describe("applyEvent / WakeupScheduled lifecycle", () => {
  it("user-typed prompt mid-wait keeps the pending wakeup", () => {
    // Regression for #1091: a user-typed follow-up during the wait
    // is NOT the wake firing. Reducer must keep `nextWakeupAt` when
    // the scheduled time is still in the future.
    const future = new Date(Date.now() + 95_000).toISOString();
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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

describe("applyEvent / SessionCleared", () => {
  // /clear wipes the model's memory. The reducer appends a divider row
  // so the renderer can fold pre-clear turns behind a disclosure
  // (#1101), and resets only the per-turn / in-flight fields the
  // cleared context invalidates. Capability caches (slash commands,
  // modes) are preserved because claude-agent-sdk caches them at
  // Query init and does not rotate them on /clear (#1128).
  it("appends a session_cleared divider row", () => {
    const next = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 5,
      event: "SessionCleared",
    });
    expect(next.activity).toHaveLength(1);
    expect(next.activity[0]).toMatchObject({
      id: "cleared-5",
      kind: "session_cleared",
    });
    expect(next.lastSeq).toBe(5);
  });

  it("resets per-turn state but preserves capability caches (#1128)", () => {
    const seeded: CockpitState = {
      ...emptyCockpitState(),
      availableCommands: [
        { name: "foo", description: "", accepts_input: false },
      ],
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
  it("appends a compacted divider row and drops the stale usage snapshot", () => {
    const seeded: CockpitState = {
      ...emptyCockpitState(),
      sessionUsage: { used: 100, size: 200_000 },
    };
    const next = applyEvent(seeded, {
      session_id: "s-1",
      seq: 9,
      event: "ConversationCompacted",
    });
    expect(next.activity).toHaveLength(1);
    expect(next.activity[0]).toMatchObject({
      id: "compacted-9",
      kind: "compacted",
    });
    expect(next.sessionUsage).toBeNull();
  });

  it("does not arm the primer banner", () => {
    // Regression: /compact previously routed through SessionContextReset
    // and the primer banner offered to pre-fill duplicate content the
    // model already had summarised. Verify the new variant doesn't
    // re-introduce that behaviour.
    const next = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 3,
      event: "ConversationCompacted",
    });
    expect(next.contextPrimerAvailable).toBeNull();
  });
});

describe("applyEvent / usageBaseline (#1354)", () => {
  // /clear and /compact do not rotate the underlying ACP session, so
  // claude-agent-acp keeps reporting session-lifetime cumulative cost
  // via UsageUpdate. The reducer captures a baseline at each boundary
  // and subtracts it from incoming UsageUpdate.cost so the composer
  // footer reads "since the most recent boundary."
  it("SessionCleared captures the cumulative cost as the baseline", () => {
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.10, currency: "USD" },
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
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 20_000,
            size: 200_000,
            cost: { amount: 0.30, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "ConversationCompacted",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.30, 6);
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.20, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.20, 6);
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.10, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.10, 6);
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
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.10, currency: "USD" },
          },
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: "SessionCleared",
    });
    expect(state.usageBaseline?.cost).toBeCloseTo(0.10, 6);
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
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        UsageUpdated: {
          usage: {
            used: 10_000,
            size: 200_000,
            cost: { amount: 0.50, currency: "USD" },
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
            cost: { amount: 0.10, currency: "USD" },
          },
        },
      },
    });
    expect(state.sessionUsage?.cost?.amount).toBe(0);
  });
});

describe("applyEvent / AgentSwitched", () => {
  // Cockpit hand-off (#1282) moves the session from one ACP backend
  // to another. Reducer must drop everything tied to the prior
  // backend so the UI doesn't show Claude's usage bar / mode pills /
  // in-flight tool while talking to Codex.
  it("clears prior-backend transient state and records the handoff", () => {
    const seeded: CockpitState = {
      ...emptyCockpitState(),
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
      availableCommands: [
        { name: "/clear", description: "wipe context", accepts_input: false },
      ],
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
    const lastRow = next.activity[next.activity.length - 1];
    expect(lastRow?.id).toBe("agent-switched-11");
    expect(lastRow?.text).toContain("claude");
    expect(lastRow?.text).toContain("codex");
  });

  it("does not double-apply on replay", () => {
    const first = applyEvent(emptyCockpitState(), {
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
    const seeded: CockpitState = {
      ...emptyCockpitState(),
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
  // The boolean field is kept on `CockpitState` as a memoised alias so
  // existing `state.turnActive` reads stay correct, but the counters
  // are the source of truth a late `Stopped` cannot clobber.

  it("isTurnActive flips on / off when counters cross", () => {
    expect(
      isTurnActive({ pendingUserPromptSeq: 2, lastStoppedSeq: 1 }),
    ).toBe(true);
    expect(
      isTurnActive({ pendingUserPromptSeq: 1, lastStoppedSeq: 1 }),
    ).toBe(false);
    expect(
      isTurnActive({ pendingUserPromptSeq: 0, lastStoppedSeq: 0 }),
    ).toBe(false);
  });

  it("Stopped advances lastStoppedSeq by one and recomputes turnActive", () => {
    // Single-prompt happy path: send → Stopped flips turnActive off.
    let state = applyEvent(emptyCockpitState(), {
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
    const { cockpitHookReducer } = await import("../hooks/useCockpit");

    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "first turn" } },
    });
    expect(state.turnActive).toBe(true);
    // User taps Send the instant the turn ends; the optimistic
    // dispatch lands BEFORE the Stopped frame for the prior turn.
    state = cockpitHookReducer(state, {
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
    let state = applyEvent(emptyCockpitState(), {
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

  it("optimistic user_prompt + matching server echo only bump pending once", async () => {
    // Avoids double-counting: the server's UserPromptSent that matches
    // and promotes an existing optimistic row must not bump
    // `pendingUserPromptSeq` again.
    const { cockpitHookReducer } = await import("../hooks/useCockpit");
    let state = cockpitHookReducer(emptyCockpitState(), {
      kind: "user_prompt",
      text: "echo me",
    });
    expect(state.pendingUserPromptSeq).toBe(1);
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 5,
      event: { UserPromptSent: { text: "echo me" } },
    });
    expect(state.pendingUserPromptSeq).toBe(1);
    expect(state.turnActive).toBe(true);
  });

  it("AgentStartupError advances lastStoppedSeq, preserving the race-safe semantics", () => {
    let state = applyEvent(emptyCockpitState(), {
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

  it("optimistic-match UserPromptSent resets per-turn flags (turnHasOutput, worker banners, wakeup)", () => {
    // The optimistic-match branch used to early-return after just
    // promoting the row id, leaving `turnHasOutput`, `workerStopped`,
    // `workerRestarting`, and the wakeup countdown stale from the
    // prior turn. With #1170's race-safe semantics that desync can
    // suppress the empty-output notice on a follow-up that produces
    // nothing, so the resets now run on BOTH UserPromptSent branches.
    const stale: CockpitState = {
      ...withOptimisticPrompt(emptyCockpitState(), "follow-up"),
      turnHasOutput: true,
      workerStopped: true,
      workerRestarting: true,
      nextWakeupAt: new Date(Date.now() - 1_000).toISOString(),
      nextWakeupReason: "tick",
    };
    const next = applyEvent(stale, {
      session_id: "s-1",
      seq: 9,
      event: { UserPromptSent: { text: "follow-up" } },
    });
    expect(next.activity).toHaveLength(1);
    expect(next.activity[0].id).toBe("user-seq-9");
    expect(next.turnHasOutput).toBe(false);
    expect(next.workerStopped).toBe(false);
    expect(next.workerRestarting).toBe(false);
    expect(next.nextWakeupAt).toBeNull();
    expect(next.nextWakeupReason).toBeNull();
    // pendingUserPromptSeq must NOT double-count: withOptimisticPrompt
    // bumped it to 1, the server echo matched the optimistic row, so
    // it stays at 1.
    expect(next.pendingUserPromptSeq).toBe(1);
    expect(next.turnActive).toBe(true);
  });
});

describe("normaliseTurnCounters (#1170 persisted-state backfill)", () => {
  it("backfills counters from cached turnActive=true", () => {
    const cached = {
      ...emptyCockpitState(),
      turnActive: true,
    } as CockpitState & { pendingUserPromptSeq?: number; lastStoppedSeq?: number };
    delete cached.pendingUserPromptSeq;
    delete cached.lastStoppedSeq;
    const normalised = normaliseTurnCounters(cached);
    expect(normalised.pendingUserPromptSeq).toBe(1);
    expect(normalised.lastStoppedSeq).toBe(0);
    expect(normalised.turnActive).toBe(true);
  });

  it("backfills counters from cached turnActive=false", () => {
    const cached = {
      ...emptyCockpitState(),
      turnActive: false,
    } as CockpitState & { pendingUserPromptSeq?: number; lastStoppedSeq?: number };
    delete cached.pendingUserPromptSeq;
    delete cached.lastStoppedSeq;
    const normalised = normaliseTurnCounters(cached);
    expect(normalised.pendingUserPromptSeq).toBe(0);
    expect(normalised.lastStoppedSeq).toBe(0);
    expect(normalised.turnActive).toBe(false);
  });

  it("passes through entries that already carry counters", () => {
    const fresh: CockpitState = {
      ...emptyCockpitState(),
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

describe("cockpitHookReducer / dismiss_primer", () => {
  // Banner dismiss used to live in component-local useState and
  // re-armed itself on every session switch. Moved into the reducer so
  // the dismissal survives mount/unmount; the next SessionContextReset
  // re-seeds contextPrimerAvailable with a new resetSeq so a later
  // incident still surfaces the banner. See #1110.
  it("clears contextPrimerAvailable", async () => {
    const { cockpitHookReducer } = await import("../hooks/useCockpit");
    const seeded: CockpitState = {
      ...emptyCockpitState(),
      contextPrimerAvailable: {
        resetSeq: 12,
        reason: "Conversation context reset; agent transcript was unavailable.",
      },
    };
    const next = cockpitHookReducer(seeded, { kind: "dismiss_primer" });
    expect(next.contextPrimerAvailable).toBeNull();
  });
});

describe("applyEvent / ModeSwitchFailed", () => {
  it("captures the rejected mode + reason", () => {
    const next = applyEvent(emptyCockpitState(), {
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
    expect(next.modeSwitchFailed?.reason).toBe(
      "Mode bypassPermissions is not available.",
    );
  });

  it("clears when a subsequent CurrentModeChanged lands", () => {
    let state = applyEvent(emptyCockpitState(), {
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

describe("cockpitHookReducer / dismiss_mode_switch_failed", () => {
  it("clears the notice", async () => {
    const { cockpitHookReducer } = await import("../hooks/useCockpit");
    const seeded: CockpitState = {
      ...emptyCockpitState(),
      modeSwitchFailed: {
        modeId: "bypassPermissions",
        reason: "denied",
        at: new Date().toISOString(),
      },
    };
    const next = cockpitHookReducer(seeded, {
      kind: "dismiss_mode_switch_failed",
    });
    expect(next.modeSwitchFailed).toBeNull();
  });
});

// Reducer coverage for the silent-orphan watchdog (#1240). The
// daemon-side detector is exercised by the Rust integration test in
// tests/cockpit_silent_orphan.rs; this block just pins down the
// frontend half so a future refactor of the worker-state banner
// doesn't silently regress the prompt_orphaned path.
function stoppedFrame(reason: string, seq: number): CockpitFrame {
  return {
    session_id: "s-orphan",
    seq,
    event: { Stopped: { reason } },
  };
}

describe("CockpitState reducer / silent-orphan watchdog (#1240)", () => {
  it("sets agentOrphaned and workerRestarting on prompt_orphaned", () => {
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
      ...emptyCockpitState(),
      pendingUserPromptSeq: 0,
      lastStoppedSeq: 0,
    } as CockpitState & { agentOrphaned?: boolean };
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
      ...emptyCockpitState(),
      pendingUserPromptSeq: 0,
      lastStoppedSeq: 0,
    } as CockpitState & { usageBaseline?: { cost: number } | null };
    delete stale.usageBaseline;
    const normalised = normaliseTurnCounters(stale);
    expect(normalised.usageBaseline).toBeNull();
  });

  it("preserves a non-null usageBaseline through normaliseTurnCounters", () => {
    // A session that ran /clear before reload writes a baseline into
    // localStorage. Hydration must keep it so post-reload UsageUpdate
    // frames continue subtracting the boundary cumulative.
    const cached: CockpitState = {
      ...emptyCockpitState(),
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    const next = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        IncompatibleAgent: {
          detail: {
            kind: "incompatible_agent_version",
            package_name: "@agentclientprotocol/claude-agent-acp",
            installed: "0.32.0",
            required: "0.39.0",
            install_command:
              "npm install -g @agentclientprotocol/claude-agent-acp@latest",
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
    let state: CockpitState = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        IncompatibleAgent: {
          detail: {
            kind: "incompatible_agent_version",
            package_name: "@agentclientprotocol/claude-agent-acp",
            installed: "0.32.0",
            required: "0.39.0",
            install_command:
              "npm install -g @agentclientprotocol/claude-agent-acp@latest",
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state: CockpitState = {
      ...emptyCockpitState(),
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
    let state = applyEvent(emptyCockpitState(), {
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
