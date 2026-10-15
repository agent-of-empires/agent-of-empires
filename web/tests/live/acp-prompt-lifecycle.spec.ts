// Structured view prompt lifecycle over REST: prompts, approvals, cancel, steering, and the event store.

import { test, expect } from "../helpers/liveTest";
import {
  HOLD,
  chunk,
  endTurn,
  postAcp,
  postPrompt,
  replayFrames,
  replayJson,
  script,
  seedAcpSession,
  startAcpSession,
  waitForReplayContains,
} from "../helpers/acp";

const expect2xx = (res: Response) => {
  expect(res.status).toBeGreaterThanOrEqual(200);
  expect(res.status).toBeLessThan(300);
};

type ApprovalFrame = {
  seq?: number;
  event?: {
    ApprovalRequested?: { approval?: { nonce?: string; tool_call?: { args_preview?: string } } };
    ToolCallStarted?: { tool_call?: { id?: string } };
    ToolCallCompleted?: { tool_call_id?: string; is_error?: boolean };
  };
};

/** Prompt a turn that requests permission and return the server-generated approval nonce. */
async function requestApproval(spawnServe: Parameters<typeof startAcpSession>[0], title: string) {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title,
    fakeAcpScript: script(
      endTurn(chunk("Considering write..."), {
        sessionUpdate: "permission_request",
        toolCall: { toolCallId: "fake-tool-call-1", title: "Write file", kind: "edit" },
      }),
    ),
  });
  await postPrompt(serve.baseUrl, sessionId, "write a file");
  const frames = async () => (await replayFrames(serve.baseUrl, sessionId)) as ApprovalFrame[];
  const nonceOf = async () => (await frames()).find((f) => f.event?.ApprovalRequested?.approval?.nonce);
  await expect.poll(nonceOf, { timeout: 15_000, intervals: [100, 200, 500, 1000] }).toBeDefined();
  const approvalFrame = (await nonceOf())!;
  const nonce = approvalFrame.event!.ApprovalRequested!.approval!.nonce!;
  const resolve = (decision: "Allow" | "Deny") =>
    // ApprovalDecisionWire is PascalCase; "allow" is a 422.
    postAcp(serve.baseUrl, sessionId, `/approvals/${nonce}`, { decision });
  return { frames, approvalFrame, resolve };
}

test("permission_request flows through to the server", async ({ spawnServe }) => {
  const { frames, approvalFrame, resolve } = await requestApproval(spawnServe, "acp-approval");
  // #1713: no raw_input means an empty preview, not the string "null".
  expect(approvalFrame.event?.ApprovalRequested?.approval?.tool_call?.args_preview).toBe("");
  // #1713: the tool card starts before the approval so it exists when the tool completes.
  const startFrame = (await frames()).find((f) => f.event?.ToolCallStarted?.tool_call?.id === "fake-tool-call-1");
  expect(startFrame).toBeDefined();
  expect(startFrame!.seq!).toBeLessThan(approvalFrame.seq!);
  expect2xx(await resolve("Allow"));
});

test("denied permission closes the tool card with an error completion (#1713)", async ({ spawnServe }) => {
  const { frames, resolve } = await requestApproval(spawnServe, "acp-approval-deny");
  expect2xx(await resolve("Deny"));
  // The denied tool never runs, so its started card must get a terminal error completion.
  await expect
    .poll(
      async () =>
        (await frames()).some(
          (f) =>
            f.event?.ToolCallCompleted?.tool_call_id === "fake-tool-call-1" &&
            f.event?.ToolCallCompleted?.is_error === true,
        ),
      { timeout: 15_000, intervals: [100, 200, 500, 1000] },
    )
    .toBe(true);
});

test("structured view/cancel publishes Stopped reason:cancelled mid-turn", async ({ spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "acp-cancel",
    fakeAcpScript: script(endTurn(chunk("Thinking..."), HOLD, chunk("MUST_NOT_COMPLETE"))),
  });
  await postPrompt(serve.baseUrl, sessionId, "long-running thought");
  await waitForReplayContains(serve.baseUrl, sessionId, "Thinking...");
  expect((await postAcp(serve.baseUrl, sessionId, "/cancel")).status).toBe(202);
  await waitForReplayContains(serve.baseUrl, sessionId, '"reason":"cancelled"');
  expect(await replayJson(serve.baseUrl, sessionId)).not.toContain("MUST_NOT_COMPLETE");
});

// #2805: a prompt during a running turn is steered when the agent supports it, otherwise queued.
test.describe("mid-turn prompts", () => {
  const heldTurn = script(endTurn(chunk("working"), HOLD));

  test("a mid-turn prompt is steered into the running turn instead of rejected", async ({ spawnServe }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: "acp-steering",
      fakeAcpScript: heldTurn,
      extraEnv: { FAKE_ACP_STEERING: "1" },
    });
    // Without the capability on the stream the daemon would take the reject path.
    await waitForReplayContains(serve.baseUrl, sessionId, '"steering":true');
    await postPrompt(serve.baseUrl, sessionId, "start the turn");
    await waitForReplayContains(serve.baseUrl, sessionId, "working");

    expect((await postPrompt(serve.baseUrl, sessionId, "also check the tests")).ok).toBe(true);
    // The fake echoes an accepted steer back into the running turn.
    await waitForReplayContains(serve.baseUrl, sessionId, "steered: also check the tests");
    expect(await replayJson(serve.baseUrl, sessionId)).not.toContain("agent_busy");
  });

  test("a mid-turn prompt is queued, not rejected, when the agent cannot be steered", async ({ spawnServe }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: "acp-no-steering",
      fakeAcpScript: heldTurn,
    });
    await postPrompt(serve.baseUrl, sessionId, "start the turn");
    await waitForReplayContains(serve.baseUrl, sessionId, "working");

    const res = await postPrompt(serve.baseUrl, sessionId, "also check the tests");
    expect(res.status).toBe(202);
    const dispatch = (await res.json()) as { disposition?: string; reason?: string; queued_id?: string };
    expect(dispatch.disposition).toBe("queued");
    expect(dispatch.reason).toBe("turn_active");

    const queue = (await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/queue`).then((r) => r.json())) as Array<{
      id: string;
      text: string;
    }>;
    expect(queue.map((q) => q.text)).toEqual(["also check the tests"]);
    expect(queue[0]!.id).toBe(dispatch.queued_id);
    expect(await replayJson(serve.baseUrl, sessionId)).not.toContain("agent_busy");
  });

  test("a prompt reaching the daemon mid-compaction is queued, not steered", async ({ spawnServe }) => {
    // #3219: steering is on, so the compaction phase is the only reason to park it.
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: "acp-compaction-rest",
      fakeAcpScript: script(endTurn(chunk("Compacting..."), HOLD, chunk("\n\nCompacting completed."))),
      extraEnv: { FAKE_ACP_STEERING: "1" },
    });
    await waitForReplayContains(serve.baseUrl, sessionId, '"steering":true');
    await postPrompt(serve.baseUrl, sessionId, "/compact");
    await waitForReplayContains(serve.baseUrl, sessionId, "ConversationCompactionStarted");

    const res = await postPrompt(serve.baseUrl, sessionId, "also check the tests");
    expect(res.status).toBe(202);
    const dispatch = (await res.json()) as { disposition?: string; reason?: string };
    expect(dispatch.disposition).toBe("queued");
    expect(dispatch.reason).toBe("compacting");
    expect(await replayJson(serve.baseUrl, sessionId)).not.toContain("steered: also check the tests");
  });
});

// force_end_turn and user prompts are published straight to the event store, so this needs no live worker.
// Replay paging is pinned by Rust event_store::replay tests (forward_pages_reassemble_the_full_replay).
test.describe("event store", () => {
  test("structured view/context-primer renders the seeded turn", async ({ spawnServe }) => {
    const primerText = "primer-fixture-prompt-1224";
    const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "acp-primer" });
    await postAcp(serve.baseUrl, sessionId, "/enable");
    await postPrompt(serve.baseUrl, sessionId, primerText);
    // The synthetic Stopped closes the turn so the primer renders it complete.
    await postAcp(serve.baseUrl, sessionId, "/force_end_turn");

    let highestSeq = 0;
    await expect
      .poll(
        async () => {
          const replay = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) =>
            r.json(),
          );
          const json = JSON.stringify(replay.frames);
          if (!json.includes(primerText) || !json.includes("user_forced") || replay.highest_seq === null) return false;
          highestSeq = replay.highest_seq;
          return true;
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBe(true);
    expect(highestSeq).toBeGreaterThan(0);

    const primerRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/context-primer?before_seq=${highestSeq + 1}`,
    );
    expect(primerRes.ok).toBeTruthy();
    const primer = (await primerRes.json()) as {
      primer: string;
      included_event_count: number;
      included_turn_count: number;
      max_chars: number;
    };
    expect(primer.included_event_count).toBeGreaterThan(0);
    expect(primer.included_turn_count).toBeGreaterThanOrEqual(1);
    expect(primer.primer).toContain(primerText);
    expect(primer.max_chars).toBeGreaterThan(0);
  });
});
