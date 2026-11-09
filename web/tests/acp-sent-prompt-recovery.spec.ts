import type { WebSocketRoute } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import {
  agentMessageChunk,
  mockAcpSession,
  openStructuredSession,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

test("a sent prompt recovers before its reply when its live transcript delta was missed", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-sent-prompt-recovery",
    initialEvents: [agentMessageChunk("An earlier completed reply."), stopped()],
    holdPromptTranscript: true,
    onPrompt: () => [agentMessageChunk("The agent already received this prompt."), stopped()],
  });
  let releaseRecovery!: () => void;
  const gate = new Promise<void>((resolve) => {
    releaseRecovery = resolve;
  });
  let requests = 0;
  await page.route(/\/acp\/replay(\?|$)/, async (route) => {
    const params = new URL(route.request().url()).searchParams;
    if (mock.promptBodies.length > 0 && params.get("since") === "2" && params.get("view") === "rows") {
      requests += 1;
      await gate;
    }
    await route.fallback();
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  const composer = page.getByRole("textbox").first();
  await composer.fill("This message has already been sent.");
  await composer.press("Enter");
  await expect.poll(() => mock.promptBodies.length).toBe(1);
  const promptId = mock.promptBodies[0]!.prompt_id!;
  const viewport = page.getByTestId("acp-viewport");
  const order = () =>
    viewport
      .locator("[data-message-id]")
      .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-message-id")));
  try {
    await expect(viewport).toContainText("The agent already received this prompt.");
    await expect(viewport.locator(`[data-message-id="${promptId}"]`)).toHaveCount(1);
    await expect.poll(() => requests).toBe(1);
    releaseRecovery();
    await expect.poll(order).toEqual(["assistant-msg-1", promptId, "assistant-msg-4"]);
    const committedText = "The late canonical transcript delta committed.";
    mock.releasePromptTranscript(committedText);
    await expect(viewport.locator(`[data-message-id="${promptId}"]`)).toContainText(committedText);
    await expect.poll(order).toEqual(["assistant-msg-1", promptId, "assistant-msg-4"]);
    await expect(viewport.locator(`[data-message-id="${promptId}"]`)).toHaveCount(1);
    expect(mock.promptBodies).toHaveLength(1);
  } finally {
    releaseRecovery();
  }
});

test("a repeated tool start keeps its original position before a recovered steering prompt", async ({ page }) => {
  const tool = {
    id: "existing-tool",
    name: "Read",
    kind: "read",
    args_preview: '{"file_path":"example.txt"}',
    started_at: "2026-01-01T00:00:00Z",
  };
  const start = {
    id: `start-${tool.id}`,
    group_id: `tool-${tool.id}`,
    kind: "tool_start",
    tool_call_id: tool.id,
    tool,
    text: "Read",
    at: tool.started_at,
  };
  const mock = await mockAcpSession(page, {
    title: "story-repeated-tool-start",
    initialEvents: [
      { PromptCapabilities: { image: false, audio: false, embedded_context: false, steering: true } },
      { ToolCallStarted: { tool_call: tool } },
    ],
    holdPromptTranscript: true,
  });
  let socket!: WebSocketRoute;
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, (route) => {
    socket = route;
  });
  let releaseRecovery!: () => void;
  const held = new Promise<void>((resolve) => {
    releaseRecovery = resolve;
  });
  let requests = 0;
  const reply = "The running tool still precedes this steering prompt.";
  await page.route(/\/acp\/replay(\?|$)/, async (route) => {
    const params = new URL(route.request().url()).searchParams;
    if (mock.promptBodies.length === 0 || params.get("since") !== "2" || params.get("view") !== "rows") {
      await route.fallback();
      return;
    }
    requests += 1;
    await held;
    await route.fulfill({
      json: {
        frames: [],
        rows: [
          { id: mock.promptBodies[0]!.prompt_id, kind: "user_prompt", text: "Steer the running turn.", at: "t" },
          start,
          { id: "msg-5", kind: "message", text: reply, at: "t" },
        ],
        highest_seq: 5,
        lost: false,
      },
    });
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  const viewport = page.getByTestId("acp-viewport");
  await expect(viewport.locator(`[data-message-id="assistant-${start.id}"]`)).toBeVisible();
  const composer = page.getByRole("textbox").first();
  await composer.fill("Steer the running turn.");
  await composer.press("Enter");
  await expect.poll(() => mock.promptBodies.length).toBe(1);
  const promptId = mock.promptBodies[0]!.prompt_id!;
  socket.send(
    JSON.stringify({
      session_id: mock.sessionId,
      seq: 3,
      event: {
        UserPromptSent: { prompt_id: promptId, text: "Steer the running turn." },
      },
    }),
  );
  socket.send(JSON.stringify({ session_id: mock.sessionId, seq: 4, event: { ToolCallStarted: { tool_call: tool } } }));
  socket.send(JSON.stringify({ kind: "transcript_delta", delta: { Patch: { id: start.id, row: start } } }));
  socket.send(JSON.stringify({ session_id: mock.sessionId, seq: 5, event: { AgentMessageChunk: { text: reply } } }));
  socket.send(
    JSON.stringify({
      kind: "transcript_delta",
      delta: { Append: { id: "msg-5", kind: "message", text: reply, at: "t" } },
    }),
  );
  try {
    await expect(viewport).toContainText(reply);
    await expect.poll(() => requests).toBe(1);
    releaseRecovery();
    await expect
      .poll(() =>
        viewport
          .locator("[data-message-id]")
          .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-message-id"))),
      )
      .toEqual([`assistant-${start.id}`, promptId, "assistant-msg-5"]);
    await expect(viewport.locator(`[data-message-id="${promptId}"]`)).toHaveCount(1);
    expect(mock.promptBodies).toHaveLength(1);
  } finally {
    releaseRecovery();
  }
});
