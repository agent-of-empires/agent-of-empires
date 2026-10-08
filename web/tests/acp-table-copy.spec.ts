import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { agentMessageChunk, mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

const WIDE = `| ${Array.from({ length: 24 }, (_, i) => `column-${i}`).join(" | ")} |`;
const WIDE_TABLE = `${WIDE}\n|${"---|".repeat(24)}\n| ${Array.from({ length: 24 }, (_, i) => `value-${i}`).join(" | ")} |`;

const MESSAGE = [
  "> Results:",
  ">",
  "> | name | count |",
  "> |:-----|------:|",
  "> | alpha | 1 |",
  "",
  "- item",
  "",
  "  | a | b |",
  "  |---|---|",
  "  | 1 | 2 |",
  "",
  WIDE_TABLE,
].join("\n");

async function openTables(page: Page) {
  const mock = await mockAcpSession(page, {
    title: "story-table-copy",
    initialEvents: [agentMessageChunk(MESSAGE), stopped()],
  });
  await openStructuredSession(page, mock);
  await expect(page.locator(".acp-markdown table")).toHaveCount(3);
}

const tableButton = (page: Page, i: number) => page.locator(".acp-table-block").nth(i).getByRole("button");

test.describe("pointer device", () => {
  test.use({ permissions: ["clipboard-read", "clipboard-write"] });

  test("button appears on hover, copies the table without its container prefix, and confirms", async ({ page }) => {
    await openTables(page);
    const quoted = tableButton(page, 0);
    const listed = tableButton(page, 1);

    await expect(quoted).toHaveCSS("opacity", "0");
    await page.locator(".acp-table-block").nth(0).hover();
    await expect(quoted).toHaveCSS("opacity", "1");

    await quoted.click();
    await expect(page.getByRole("button", { name: "Copied" })).toBeVisible();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(
      "| name | count |\n|:-----|------:|\n| alpha | 1 |",
    );

    await page.locator(".acp-table-block").nth(1).hover();
    await listed.click();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe("| a | b |\n|---|---|\n| 1 | 2 |");
  });

  test("button stays in place while a wide table scrolls", async ({ page }) => {
    await openTables(page);
    const block = page.locator(".acp-table-block").nth(2);
    await block.hover();
    const button = tableButton(page, 2);
    const before = await button.boundingBox();

    await block.locator(".acp-table-wrap").evaluate((el) => {
      el.scrollLeft = el.scrollWidth;
    });
    const after = await button.boundingBox();
    expect(after!.x).toBeCloseTo(before!.x, 0);
    await expect(button).toBeVisible();
  });
});

test.describe("touch device", () => {
  test.use({
    viewport: devices["iPhone 13"].viewport,
    hasTouch: true,
    isMobile: true,
    permissions: ["clipboard-read", "clipboard-write"],
  });

  test("button is visible without hover and copies on tap", async ({ page }) => {
    await openTables(page);
    const button = tableButton(page, 0);
    await expect(button).toHaveCSS("opacity", "1");
    await button.tap();
    await expect(page.getByRole("button", { name: "Copied" })).toBeVisible();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(
      "| name | count |\n|:-----|------:|\n| alpha | 1 |",
    );
  });
});
