import { test, expect } from "./helpers/mockedTest";
import { agentMessageChunk, mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

const REM = 16;

test("the transcript column keeps widening on wide screens, with the composer aligned to it", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-wide-screen",
    initialEvents: [agentMessageChunk("x ".repeat(2000)), stopped()],
  });
  await openStructuredSession(page, mock);
  const column = page.getByTestId("acp-viewport").locator(":scope > div").first();
  const composer = page.locator("[data-session-composer]");
  await expect(column).toBeVisible();
  await expect(composer).toBeVisible();

  for (const { viewport, cap } of [
    { viewport: 1440, cap: 56 },
    { viewport: 1700, cap: 64 },
    { viewport: 2000, cap: 72 },
    { viewport: 2560, cap: 80 },
  ]) {
    await page.setViewportSize({ width: viewport, height: 1000 });
    await expect.poll(async () => (await column.boundingBox())?.width, { message: `${viewport}px` }).toBe(cap * REM);
    const [c, k] = [(await column.boundingBox())!, (await composer.boundingBox())!];
    expect({ x: k.x, width: k.width }).toEqual({ x: c.x, width: c.width });
  }
});
