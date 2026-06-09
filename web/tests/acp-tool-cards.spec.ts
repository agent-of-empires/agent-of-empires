// Mocked ports of the live acp-stories tool-card specs, replaying
// canned ACP frames instead of standing up `aoe serve` plus the fake
// agent. Consolidates:
//   - edit-card-diff-scroll (#1568)
//   - fold-failed-tool-card (#1467)

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, toolCallStarted, toolCallCompleted, stopped } from "./helpers/acpMock";

// User story (#1568): the diff embedded inside the structured view
// Edit/Write tool card scrolls horizontally so a line wider than the
// card is reachable on a narrow (mobile) viewport.
//
// The edit tool_call's new_string carries a line far wider than the
// card. On a 480px viewport the expanded card body used to clip that
// line: `CardChrome` wraps the body in `overflow-hidden`, `DiffLine`
// content is `whitespace-pre`, and nothing in between gave a scroll
// context. The fix adds `overflow-x-auto` to `StringDiff`'s container
// (mirroring the full-size `DiffFileViewer` wrapper), so the diff body
// scrolls while the card chrome and the transcript viewport stay put.
test.describe("edit card diff scroll", () => {
  // Narrow viewport so the long line is wider than the card.
  test.use({ viewport: { width: 480, height: 800 } });

  // A single new-string line far wider than a 480px card, with no break
  // opportunities, so `whitespace-pre` forces it past the card edge.
  const LONG_LINE = `const x = "${"a".repeat(300)}";`;

  test("edit card diff scrolls horizontally on a narrow viewport", async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-edit-scroll",
      initialEvents: [
        toolCallStarted({
          id: "tc-edit-1",
          name: "Edit",
          kind: "edit",
          args_preview: JSON.stringify({
            file_path: "big.txt",
            old_string: "const x = 1;",
            new_string: LONG_LINE,
          }),
        }),
      ],
    });
    await openStructuredSession(page, mock);

    // The edit card renders collapsed; its header carries the file path.
    const cardHeader = page.getByRole("button").filter({ hasText: "big.txt" }).first();
    await expect(cardHeader).toBeVisible({ timeout: 10_000 });
    await cardHeader.click();

    // The diff body is now expanded.
    const diff = page.getByTestId("string-diff");
    await expect(diff).toBeVisible({ timeout: 10_000 });

    // Core regression: the diff container is an `overflow-x` scroll context
    // (pre-fix it was the default `visible`, so the long line was clipped by
    // the card's `overflow-hidden` and unreachable).
    const overflowX = await diff.evaluate((el) => getComputedStyle(el).overflowX);
    expect(["auto", "scroll"]).toContain(overflowX);

    // And the content actually overflows that container, so the scroll
    // affordance is real rather than vacuous.
    await expect
      .poll(async () => diff.evaluate((el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth))
      .toBeGreaterThan(0);

    // Chrome stays put: scrolling lives on the diff body, not the transcript
    // viewport, so the whole panel never gains a horizontal scrollbar.
    const viewport = page.getByTestId("acp-viewport");
    await expect(viewport).toBeVisible();
    await expect
      .poll(async () => viewport.evaluate((el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth))
      .toBeLessThanOrEqual(0);
  });
});

// User story (#1467): a failed tool card auto-opens so the error is
// visible, but the header chevron must still fold it once the user has
// read it.
//
// A tool_call frame renders the card, then a failed completion frame
// carries the error text. `src/acp/acp_client.rs` maps the failed
// update to a tool_error row, so the web `statusFor` resolves to "err"
// and the card opens on its own. Clicking the header collapses the rose
// error block; clicking again re-expands it.
test("failed tool card auto-opens and folds via the chevron", async ({ page }) => {
  const ERROR_TEXT = "boom: the command exploded";
  const mock = await mockAcpSession(page, {
    title: "story-fold-fail",
    initialEvents: [
      toolCallStarted({
        id: "tc-fail-1",
        name: "Terminal",
        kind: "execute",
        args_preview: JSON.stringify({ command: "rm -rf /nope" }),
      }),
      toolCallCompleted({
        tool_call_id: "tc-fail-1",
        is_error: true,
        content: ERROR_TEXT,
      }),
      stopped(),
    ],
  });
  await openStructuredSession(page, mock);

  // The failed card auto-opens: both the rose "tool failed" label and
  // the error text are visible without any user interaction.
  const errorText = page.getByText(ERROR_TEXT);
  await expect(errorText).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("tool failed")).toBeVisible();

  // The card header is the only button carrying the "failed" status
  // badge; clicking it folds the body.
  const cardHeader = page
    .getByRole("button")
    .filter({ hasText: /failed/i })
    .first();
  await cardHeader.click();
  await expect(errorText).toBeHidden({ timeout: 10_000 });

  // Clicking again re-expands it.
  await cardHeader.click();
  await expect(errorText).toBeVisible({ timeout: 10_000 });
});
