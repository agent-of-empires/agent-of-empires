import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import {
  agentMessageChunk,
  mockAcpSession,
  openStructuredSession,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

// The composer grows with its content up to a cap, and typing must not strand a bottom-pinned transcript.
async function typeUntilCapKeepsPin(page: Page, inlineHeight: string) {
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
  const height = () => composer.evaluate((el) => el.getBoundingClientRect().height);
  const initialHeight = await height();
  const gaps: number[] = [];
  for (let line = 0; line < 12; line++) {
    await composer.pressSequentially(`draft line ${line}`);
    await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    gaps.push(await gap());
    await composer.press("Shift+Enter");
  }
  expect(Math.max(...gaps)).toBeLessThanOrEqual(1);
  expect(initialHeight).toBeLessThan(100);
  expect(await height()).toBe(200);
  expect(await composer.evaluate((el) => (el as HTMLElement).style.height)).toBe(inlineHeight);
  expect(await composer.evaluate((el) => el.scrollHeight > el.clientHeight)).toBe(true);
}

test("typing a multi-line draft grows the composer to its cap and keeps the transcript pinned", async ({ page }) => {
  await typeUntilCapKeepsPin(page, "");
});

// Where CSS field-sizing is unsupported, JS sizes the composer (an inline height).
test("the JS fallback grows the composer to its cap and keeps the transcript pinned", async ({ page }) => {
  await page.addInitScript(() => {
    const supports = CSS.supports.bind(CSS) as (...args: string[]) => boolean;
    CSS.supports = (...args: string[]) => !String(args[0]).includes("field-sizing") && supports(...args);
  });
  // Chromium still honors the field-sizing-content class, so neutralize it to make JS the only sizer.
  await page.addInitScript(() => {
    document.addEventListener("DOMContentLoaded", () => {
      const style = document.createElement("style");
      style.textContent = "textarea { field-sizing: fixed !important }";
      document.head.appendChild(style);
    });
  });
  await typeUntilCapKeepsPin(page, "200px");
});
