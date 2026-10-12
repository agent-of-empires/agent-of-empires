import { test, expect } from "./helpers/mockedTest";
import {
  agentMessageChunk,
  mockAcpSession,
  openStructuredSession,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

// Typing into the composer must not strand a bottom-pinned transcript.
test("typing a multi-line draft keeps the transcript pinned to the bottom", async ({ page }) => {
  const history = Array.from({ length: 120 }, (_, i) => `history line ${i}`).join("\n\n");
  const mock = await mockAcpSession(page, {
    title: "typing-keeps-pin",
    initialEvents: [agentMessageChunk(history), stopped()],
  });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);

  const viewport = page.getByTestId("acp-viewport");
  const gap = () => viewport.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
  await expect.poll(() => viewport.evaluate((el) => el.scrollHeight > el.clientHeight + 40)).toBe(true);
  await expect.poll(gap).toBeLessThanOrEqual(1);

  const composer = page.getByRole("textbox").first();
  await composer.focus();
  const gaps: number[] = [];
  for (let line = 0; line < 6; line++) {
    await composer.pressSequentially(`draft line ${line}`);
    await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    gaps.push(await gap());
    await composer.press("Shift+Enter");
  }
  expect(Math.max(...gaps)).toBeLessThanOrEqual(1);
});
