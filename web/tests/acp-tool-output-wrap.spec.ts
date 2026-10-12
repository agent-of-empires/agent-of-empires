import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

// The wrap toggle must work by tap alone: phones have neither hover nor keyboard focus.
test.use({ ...devices["iPhone 13"] });

const LONG = "word-".repeat(120);

async function openToolCard(page: Page, about?: Record<string, unknown>) {
  const mock = await mockAcpSession(page, {
    title: "story-tool-wrap",
    about,
    initialEvents: [
      {
        ToolCallStarted: {
          tool_call: {
            id: "wrap-1",
            name: "mcp__demo__echo",
            kind: "other",
            args_preview: JSON.stringify({ text: LONG }),
            started_at: new Date().toISOString(),
          },
        },
      },
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);

  await page.getByRole("button").filter({ hasText: "Echo" }).first().tap();
}

test("tool output wrap toggle is always visible and wraps long lines on tap", async ({ page }) => {
  await openToolCard(page);

  const toggle = page.getByRole("button", { name: "Wrap", exact: true });
  await expect(toggle).toBeVisible();
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  expect((await toggle.boundingBox())!.height).toBeGreaterThanOrEqual(32);

  const block = page.locator("pre").filter({ hasText: "word-word-" }).first();
  const overflows = () => block.evaluate((el) => el.scrollWidth > el.clientWidth);
  expect(await overflows()).toBe(true);

  await toggle.tap();
  await expect(toggle).toHaveAttribute("aria-pressed", "true");
  expect(await overflows()).toBe(false);

  const line = block.locator(".wrap-line").filter({ hasText: "word-word-" }).first();
  const rows = await line.evaluate(
    (el) => el.getBoundingClientRect().height / parseFloat(getComputedStyle(el).lineHeight),
  );
  expect(rows).toBeGreaterThan(1.5);

  // The corner-arrow gutter marker is drawn from the line's pseudo-element.
  const marker = await line.evaluate((el) => {
    const style = getComputedStyle(el, "::before");
    return { content: style.content, top: style.top, width: style.width };
  });
  expect(marker.content).toBe('""');
  expect(parseFloat(marker.width)).toBeGreaterThan(0);

  await toggle.tap();
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  expect(await overflows()).toBe(true);
});

// The setting reaches the block through /api/about, not through injected props.
test("acp_wrap_tool_output from /api/about makes blocks start wrapped", async ({ page }) => {
  await openToolCard(page, { acp_wrap_tool_output: true });

  const toggle = page.getByRole("button", { name: "Wrap", exact: true });
  await expect(toggle).toHaveAttribute("aria-pressed", "true");
  const block = page.locator("pre").filter({ hasText: "word-word-" }).first();
  await expect(block).toHaveClass(/wrap-lines/);
  expect(await block.evaluate((el) => el.scrollWidth > el.clientWidth)).toBe(false);
});
