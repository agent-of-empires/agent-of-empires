import { test, expect } from "./helpers/mockedTest";
import {
  agentMessageChunk,
  mockAcpSession,
  openStructuredSession,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

// User story (#3993): on a desktop browser the reader is no longer stuck to the
// bottom (they scrolled up, or a growing composer dropped the pin). Sending a
// prompt returns the transcript to the bottom and the streamed reply follows.
test("sending a prompt re-pins the transcript and follows the reply", async ({ page }) => {
  const history = Array.from({ length: 120 }, (_, i) => `history line ${i}`).join("\n\n");
  const mock = await mockAcpSession(page, {
    title: "story-submit-repin",
    initialEvents: [agentMessageChunk(history), stopped()],
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const viewport = page.getByTestId("acp-viewport");
  const isPinned = () => viewport.evaluate((el) => el.scrollTop + el.clientHeight >= el.scrollHeight - 16);
  await expect.poll(() => viewport.evaluate((el) => el.scrollHeight > el.clientHeight + 40)).toBe(true);
  await viewport.evaluate((el) => {
    el.dispatchEvent(new WheelEvent("wheel", { deltaY: -300, bubbles: true }));
    el.scrollTop = 0;
  });
  await expect.poll(isPinned).toBe(false);

  const composer = page.getByRole("textbox").first();
  await composer.fill("follow-up question");
  await composer.press("Enter");
  await expect(viewport).toContainText("follow-up question");

  for (let i = 0; i < 10; i++) {
    mock.pushEvents([agentMessageChunk(`\n\nreply paragraph ${i}`)]);
    await expect(viewport).toContainText(`reply paragraph ${i}`);
  }
  await expect.poll(isPinned).toBe(true);
});
