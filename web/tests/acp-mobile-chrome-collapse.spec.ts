import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";
import { agentMessageChunk, mockAcpSession, openStructuredSession, waitForComposerConnected } from "./helpers/acpMock";

// User story: on a phone the top bar and the composer eat a third of the
// screen. Each gets its own handle that folds it away and hands the freed
// height to the transcript; the handles stay tappable in both states so the
// user is never stranded in a collapsed layout.
//
// Mocked (not live) because everything asserted here is client-side layout:
// no backend state is involved. One test walks all four combinations rather
// than four tests, per the repo's "one test per behavior" rule.
test.use({ ...devices["iPhone 13"] });

test.describe("mobile conversation chrome collapse", () => {
  test("header and composer collapse independently and hand their height to the transcript", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-collapse",
      initialEvents: [agentMessageChunk("hello from the agent")],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const headerToggle = page.getByTestId("header-collapse-toggle");
    const composerToggle = page.getByTestId("composer-collapse-toggle");

    // Heights, not visibility: a collapsed region clips a child that still has
    // a box of its own, so Playwright would report the child as visible. The
    // contract is that the *row* releases its layout height.
    const heightOf = async (testId: string) => (await page.getByTestId(testId).boundingBox())!.height;
    const viewportHeight = () => heightOf("acp-viewport");

    await expect(page.getByTestId("composer-footer")).toBeVisible();

    // Type before collapsing: the draft has to survive the round trip, which is
    // what keeps the composer mounted rather than unmounted while hidden. Done
    // before the baseline heights because the textarea sizes to its content.
    const draft = page.getByRole("textbox").first();
    await draft.fill("half-written prompt");

    const headerHeight = await heightOf("conversation-header");
    const composerHeight = await heightOf("conversation-composer");
    expect(headerHeight).toBeGreaterThan(0);
    expect(composerHeight).toBeGreaterThan(0);
    const bothExpanded = await viewportHeight();

    // Composer collapsed, header still expanded.
    await expect(composerToggle).toHaveAttribute("aria-label", "Collapse message composer");
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-composer")).toBe(0);
    expect(await heightOf("conversation-header")).toBe(headerHeight);
    await expect(composerToggle).toHaveAttribute("aria-label", "Expand message composer");
    // `inert` in a real browser, not just the property jsdom can report: the
    // hidden composer takes neither a tap on the transcript (which normally
    // focuses it) nor a direct focus() call. Driven through the DOM because an
    // inert subtree is off the accessibility tree, so role queries cannot see
    // it while collapsed.
    await page.mouse.click(150, 250);
    const focusedAfter = await page.evaluate(() => {
      document.querySelector<HTMLTextAreaElement>("textarea")?.focus();
      return document.activeElement?.tagName ?? null;
    });
    expect(focusedAfter).not.toBe("TEXTAREA");
    const composerOnly = await viewportHeight();
    expect(composerOnly).toBeCloseTo(bothExpanded + composerHeight, 0);

    // Both collapsed: the handles are still on screen and tappable.
    await headerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect(headerToggle).toBeVisible();
    await expect(composerToggle).toBeVisible();
    const bothCollapsed = await viewportHeight();
    expect(bothCollapsed).toBeCloseTo(composerOnly + headerHeight, 0);

    // Header collapsed, composer restored: the two states are independent.
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-composer")).toBe(composerHeight);
    expect(await heightOf("conversation-header")).toBe(0);
    expect(await viewportHeight()).toBeLessThan(bothCollapsed);

    // Back to both expanded, and the composer still works after the round trip:
    // the draft is intact and sends.
    await headerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(headerHeight);
    expect(await viewportHeight()).toBeCloseTo(bothExpanded, 0);

    await expect(draft).toHaveValue("half-written prompt");
    await draft.fill("still typing");
    await page.getByRole("button", { name: "Send message" }).click();
    await expect.poll(() => mock.promptBodies.map((b) => b.text)).toContain("still typing");
  });

  // Regression for the hit-target hazard: the handles are overlays, so any
  // clickable area wider than the tab the user can see silently eats taps on
  // whatever sits underneath. An earlier revision wrapped the 28x16 tab in an
  // invisible 32x32 button and made the update banner's dismiss control (same
  // corner) untappable for as long as a release was pending.
  test("each handle's clickable area is the tab you can see, so it intercepts nothing around it", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-collapse-hit",
      initialEvents: [agentMessageChunk("hello from the agent")],
    });
    await page.route("**/api/system/update-status", (r) =>
      r.fulfill({
        json: {
          update_check_mode: "notify",
          current_version: "0.5.0",
          latest_version: "0.6.0",
          update_available: true,
          release_url: "https://example.invalid/releases/v0.6.0",
          error: null,
        },
      }),
    );
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    // What `elementFromPoint` reports at a viewport point, as a handle test id
    // (the SVG glyph is a child of the button, so walk up to the button).
    const handleAt = (x: number, y: number) =>
      page.evaluate(
        ([px, py]) =>
          document
            .elementFromPoint(px, py)
            ?.closest<HTMLElement>("[data-testid$='-collapse-toggle']")
            ?.getAttribute("data-testid") ?? null,
        [x, y],
      );

    for (const testId of ["header-collapse-toggle", "composer-collapse-toggle"]) {
      const box = (await page.getByTestId(testId).boundingBox())!;
      // The 28x16 tab of the accepted design. Asserted because the geometry is
      // the behavior: this is what the user aims at and all they can hit.
      expect({ width: box.width, height: box.height }).toEqual({ width: 28, height: 16 });
      // The clickable element is the painted one. Without this, a transparent
      // button wrapping a smaller painted tab would still measure "as big as
      // it looks" below, because every measurement would be the wrapper's.
      const painted = await page
        .getByTestId(testId)
        .evaluate((el) => getComputedStyle(el).backgroundColor !== "rgba(0, 0, 0, 0)");
      expect(painted, `${testId} paints its own hit area`).toBe(true);
      // Inside the tab it is the handle; a few pixels outside it, on every
      // side, the handle is already out of the way.
      expect(await handleAt(box.x + box.width / 2, box.y + box.height / 2)).toBe(testId);
      const outside = [
        [box.x + box.width / 2, box.y - 6],
        [box.x + box.width / 2, box.y + box.height + 6],
        [box.x - 6, box.y + box.height / 2],
        [box.x + box.width + 6, box.y + box.height / 2],
      ] as const;
      for (const [x, y] of outside) {
        expect(await handleAt(x, y), `${testId} at ${x},${y}`).toBeNull();
      }
    }

    // The concrete collision: the banner's dismiss control shares the header
    // handle's corner. A real click asserts interception, which a geometry
    // check alone would not (Playwright fails the click if anything covers it).
    const dismiss = page.getByRole("button", { name: "Dismiss update notice" });
    await dismiss.click({ timeout: 5_000 });
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  // The feature is phone-only, and the header region is now wrapped on every
  // view rather than only the collapsible ones, so "desktop is untouched" is a
  // claim this branch has to keep making. Crossing the breakpoint is the same
  // behavior from the other side: the chrome must come back, unforced, without
  // dropping what the user had typed.
  test("desktop renders no handles, and crossing the breakpoint restores the chrome", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-collapse-desktop",
      initialEvents: [agentMessageChunk("hello from the agent")],
    });
    await openStructuredSession(page, mock);
    await waitForComposerConnected(page);

    const headerToggle = page.getByTestId("header-collapse-toggle");
    const composerToggle = page.getByTestId("composer-collapse-toggle");
    const heightOf = async (testId: string) => (await page.getByTestId(testId).boundingBox())!.height;

    // Type, collapse both on the phone, then cross `md`. A collapsed composer
    // is off the accessibility tree, so its value is read through the DOM.
    await page.getByRole("textbox").first().fill("typed on the phone");
    const draftValue = () => page.evaluate(() => document.querySelector("textarea")?.value ?? null);
    await headerToggle.click();
    await composerToggle.click();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect.poll(() => heightOf("conversation-composer")).toBe(0);

    await page.setViewportSize({ width: 1280, height: 800 });
    await expect(headerToggle).toHaveCount(0);
    await expect(composerToggle).toHaveCount(0);
    // Neither region stays folded: desktop has room for both, and the top bar
    // is the only navigation the other views have.
    await expect.poll(() => heightOf("conversation-header")).toBeGreaterThan(0);
    await expect.poll(() => heightOf("conversation-composer")).toBeGreaterThan(0);
    await expect(page.getByRole("textbox").first()).toHaveValue("typed on the phone");

    // Back under `md`: the handles return, and the draft is still there. The
    // two collapse states are deliberately scoped differently, so they come
    // back differently. Header state lives in `App`, which spans the
    // breakpoint, so it is still collapsed. Composer state is local to the
    // structured view, and crossing `md` swaps App's whole mobile pane for the
    // desktop split, so the view remounts and the composer returns expanded.
    await page.setViewportSize({ width: 390, height: 664 });
    await expect(headerToggle).toBeVisible();
    await expect(composerToggle).toBeVisible();
    await expect.poll(() => heightOf("conversation-header")).toBe(0);
    await expect.poll(() => heightOf("conversation-composer")).toBeGreaterThan(0);
    expect(await draftValue()).toBe("typed on the phone");
  });
});
