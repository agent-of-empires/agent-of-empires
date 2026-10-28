import type { Locator, Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { copiedBy } from "./helpers/clipboard";
import { agentMessageChunk, mockAcpSession, openStructuredSession, stopped, toolCallStarted } from "./helpers/acpMock";

const SHOTS = process.env.MESSAGE_COPY_SHOTS;

async function shot(page: Page, name: string) {
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${name}.png` });
}

// Long enough to wrap, so its first lines reach the right edge where the button sits.
const FIRST_PART = `First part with **bold** and \`code\`. ${"More words to fill the line. ".repeat(20).trim()}`;

/** Whether any rendered text of the text part sits under its copy button. */
async function textUnderButton(page: Page, i: number): Promise<boolean> {
  return innerButton(page, i).evaluate((button) => {
    const box = button.getBoundingClientRect();
    const walker = document.createTreeWalker(
      button.closest(".acp-copy-host")!.querySelector(".acp-markdown")!,
      NodeFilter.SHOW_TEXT,
    );
    const range = document.createRange();
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      range.selectNodeContents(node);
      const hit = [...range.getClientRects()].some(
        (r) => r.width > 0 && r.right > box.left && r.left < box.right && r.bottom > box.top && r.top < box.bottom,
      );
      if (hit) return true;
    }
    return false;
  });
}

async function openMessage(page: Page) {
  const mock = await mockAcpSession(page, {
    title: "story-message-copy",
    initialEvents: [
      agentMessageChunk(FIRST_PART),
      toolCallStarted({ id: "t1", name: "Read", kind: "read", args_preview: '{"path":"a.rs"}' }),
      agentMessageChunk("Second part.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n"),
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);
  await expect(page.getByRole("button", { name: "Copy text as markdown" })).toHaveCount(2);
}

const innerButton = (page: Page, i: number) => page.getByRole("button", { name: "Copy text as markdown" }).nth(i);
/** The reveal class sits on the footer row around a button, not on the button. */
const revealOf = (button: Locator) => button.locator("xpath=ancestor::div[contains(@class, 'acp-copy-reveal')][1]");
const outerButton = (page: Page) => page.getByRole("button", { name: "Copy message as markdown" });

test.describe("pointer device", () => {
  test.use({ permissions: ["clipboard-read", "clipboard-write"] });

  test("buttons appear on hover, and each copies its scope", async ({ page }) => {
    await openMessage(page);
    await expect(revealOf(innerButton(page, 0))).toHaveCSS("opacity", "0");
    const outerWrapper = revealOf(outerButton(page));
    await expect(outerWrapper).toHaveCSS("opacity", "0");
    await shot(page, "desktop-idle");

    await page.getByText("First part with").hover();
    await expect(revealOf(innerButton(page, 0))).toHaveCSS("opacity", "1");
    // The last part's button shares the message footer row, which reveals with it.
    await expect(outerWrapper.getByRole("button", { name: "Copy text as markdown" })).toHaveCount(1);
    await expect(outerWrapper).toHaveCSS("opacity", "1");
    await shot(page, "desktop-hover-first-part");

    // Neither button may sit on top of the other or of the text it copies.
    const a = (await innerButton(page, 0).boundingBox())!;
    const b = (await outerButton(page).boundingBox())!;
    const disjoint = a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y;
    expect(disjoint).toBe(true);
    expect(await textUnderButton(page, 0)).toBe(false);

    expect(await copiedBy(page, () => innerButton(page, 0).click())).toBe(FIRST_PART);
    await expect(page.getByRole("button", { name: "Copied" })).toBeVisible();

    // "Second part." is only in the whole message, so a copy of just the first part would fail here.
    const message = await copiedBy(page, () => outerButton(page).click());
    expect(message).toContain("First part with");
    expect(message).toContain("Second part.");
    expect(message).not.toContain("**Read**");
  });

  test("a clicked copy button keeps focus but its row hides again once the pointer leaves", async ({ page }) => {
    await openMessage(page);
    // Found by position, not by label: the label reads "Copied" for a moment after the click, and a locator
    // by name would then resolve to a different button.
    const firstPartRow = page.locator(".acp-copy-host:has(> .acp-markdown) > .acp-copy-reveal").first();
    const firstPartButton = firstPartRow.getByRole("button");
    await page.getByText("First part with").hover();
    await firstPartButton.click();
    await expect(firstPartButton).toHaveAccessibleName("Copied");

    await page.mouse.move(0, 0);
    await expect(firstPartButton).toBeFocused();
    await expect(firstPartRow).toHaveCSS("opacity", "0");
    await expect(revealOf(outerButton(page))).toHaveCSS("opacity", "0");
  });

  test("a turn that is still running already offers both copies", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-message-copy-running",
      // No Stopped event: the turn stays live after its tool call starts.
      initialEvents: [
        agentMessageChunk("Done so far."),
        toolCallStarted({ id: "t1", name: "Read", kind: "read", args_preview: '{"path":"a.rs"}' }),
      ],
    });
    await openStructuredSession(page, mock);
    await page.getByText("Done so far.").hover();
    await expect(innerButton(page, 0)).toBeVisible();
    await shot(page, "desktop-running");

    expect(await copiedBy(page, () => innerButton(page, 0).click())).toBe("Done so far.");
    expect(await copiedBy(page, () => outerButton(page).click())).toBe("Done so far.");
  });

  test("the menu is keyboard operable: focus moves in, and Escape or a selection returns it to the trigger", async ({
    page,
  }) => {
    await openMessage(page);
    const trigger = page.getByRole("button", { name: "More copy options" });
    const first = page.getByRole("menuitem", { name: "Copy text" });
    const second = page.getByRole("menuitem", { name: "Copy with tool calls" });

    await trigger.focus();
    await page.keyboard.press("Enter");
    await expect(first).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(second).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(first).toBeFocused();
    await page.keyboard.press("ArrowUp");
    await expect(second).toBeFocused();
    await page.keyboard.press("Home");
    await expect(first).toBeFocused();
    await page.keyboard.press("End");
    await expect(second).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect(trigger).toBeFocused();

    // ArrowDown on the trigger opens it too, and a selection puts focus back on the trigger.
    await page.keyboard.press("ArrowDown");
    await expect(first).toBeFocused();
    await page.keyboard.press("ArrowDown");
    const copied = await copiedBy(page, () => page.keyboard.press("Enter"));
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect(trigger).toBeFocused();
    expect(copied).toContain("**Read**");
  });

  test("the menu copies tool calls too", async ({ page }) => {
    await openMessage(page);
    await page.getByText("First part with").hover();
    await page.getByRole("button", { name: "More copy options" }).click();
    await expect(page.getByRole("menu")).toBeVisible();
    await shot(page, "desktop-menu-open");

    const copied = await copiedBy(page, () => page.getByRole("menuitem", { name: "Copy with tool calls" }).click());
    await expect(page.getByRole("menu")).toHaveCount(0);
    expect(copied).toContain("**Read**");
    expect(copied).toContain('"path": "a.rs"');
    expect(copied.indexOf("First part")).toBeLessThan(copied.indexOf("**Read**"));
    expect(copied.indexOf("**Read**")).toBeLessThan(copied.indexOf("Second part"));
  });
});

test.describe("touch device", () => {
  test.use({
    viewport: devices["iPhone 13"].viewport,
    hasTouch: true,
    isMobile: true,
    permissions: ["clipboard-read", "clipboard-write"],
  });

  test("buttons are visible without hover and big enough to tap", async ({ page }) => {
    await openMessage(page);
    await expect(revealOf(innerButton(page, 0))).toHaveCSS("opacity", "1");
    for (const button of [outerButton(page), innerButton(page, 0)]) {
      const bounds = await button.boundingBox();
      expect(bounds!.width).toBeGreaterThanOrEqual(32);
      expect(bounds!.height).toBeGreaterThanOrEqual(32);
    }
    expect(await textUnderButton(page, 0)).toBe(false);
    await shot(page, "mobile");
    expect(await copiedBy(page, () => innerButton(page, 1).tap())).toContain("Second part.");
  });
});
