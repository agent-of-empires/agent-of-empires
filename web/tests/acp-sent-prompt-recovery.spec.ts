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
