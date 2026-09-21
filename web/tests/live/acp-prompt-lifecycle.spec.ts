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

test("structured view spawn + prompt round-trip emits an agent_message_chunk", async ({ spawnServe }) => {
  // Enable spawns the worker itself; an explicit /acp/spawn would 409.
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "acp-trace" });
  expect2xx(await postPrompt(serve.baseUrl, sessionId, "hello structured view"));
  await waitForReplayContains(serve.baseUrl, sessionId, ["agent_message_chunk", "AgentMessageChunk"]);
});

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

// force_end_turn and user prompts are published straight to the event store, so these need no live worker.
test.describe("event store", () => {
  test("structured view/force_end_turn publishes a synthetic Stopped event", async ({ spawnServe }) => {
    const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "acp-force-end" });
    expect((await postAcp(serve.baseUrl, sessionId, "/enable")).ok).toBeTruthy();
    expect((await postAcp(serve.baseUrl, sessionId, "/force_end_turn")).status).toBe(202);
    await waitForReplayContains(serve.baseUrl, sessionId, "user_forced");
  });

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

  test("structured view/replay surfaces seeded events and signals lost frames", async ({ spawnServe }) => {
    const seedEvents = 5;
    // Keep the worker stopped so startup frames cannot land between head snapshot and tail probe.
    const { serve, sessionId } = await seedAcpSession(spawnServe, { title: "acp-replay" });
    for (let i = 0; i < seedEvents; i++) {
      expect((await postAcp(serve.baseUrl, sessionId, "/force_end_turn")).status).toBe(202);
    }

    type Replay = { frames: { seq: number }[]; lost: boolean; highest_seq: number | null; lowest_seq: number | null };
    const replay = (query: string) =>
      fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?${query}`).then((r) => r.json());
    let body: Replay | null = null;
    await expect
      .poll(
        async () => {
          body = (await replay("since=0")) as Replay;
          return JSON.stringify(body.frames).split('"user_forced"').length - 1;
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBeGreaterThanOrEqual(seedEvents);
    const full = body as Replay | null;
    expect(full).not.toBeNull();
    expect(full!.frames.length).toBeGreaterThanOrEqual(seedEvents);
    expect(full!.lowest_seq).not.toBeNull();
    expect(full!.highest_seq).not.toBeNull();
    expect(full!.lost).toBe(false);
    for (let i = 1; i < full!.frames.length; i++) {
      expect(full!.frames[i]!.seq).toBeGreaterThan(full!.frames[i - 1]!.seq);
    }

    const highest = full!.highest_seq!;
    const tail = await replay(`since=${highest}`);
    expect(tail.frames.length).toBe(0);
    expect(tail.highest_seq).toBe(highest);
    expect(tail.lost).toBe(false);

    // Following next_cursor with a small limit reassembles the unbounded transcript, capped at the snapshot head.
    const fullSeqs = full!.frames.map((f) => f.seq).filter((s) => s <= highest);
    const pageSize = 2;
    const pagedSeqs: number[] = [];
    let cursor = 0;
    let pages = 0;
    for (;;) {
      const page = (await replay(`since=${cursor}&limit=${pageSize}`)) as Replay & {
        next_cursor: number | null;
        has_more: boolean;
      };
      pages++;
      expect(page.frames.length).toBeLessThanOrEqual(pageSize);
      pagedSeqs.push(...page.frames.map((f) => f.seq).filter((s) => s <= highest));
      const next = page.next_cursor;
      if (!(page.has_more && next != null && next > cursor && next < highest)) break;
      cursor = next;
    }
    expect(pages).toBeGreaterThan(1);
    expect(pagedSeqs).toEqual(fullSeqs);
  });
});
