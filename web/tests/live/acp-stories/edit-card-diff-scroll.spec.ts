// User story (#1568): the diff embedded inside the structured view Edit/Write tool
// card scrolls horizontally so a line wider than the card is reachable on a
// narrow (mobile) viewport.
//
// The fake ACP agent emits an `edit` tool_call whose new_string carries a line
// far wider than the card. On a 480px viewport the expanded card body used to
// clip that line: `CardChrome` wraps the body in `overflow-hidden`, `DiffLine`
// content is `whitespace-pre`, and nothing in between gave a scroll context.
// The fix adds `overflow-x-auto` to `StringDiff`'s container (mirroring the
// full-size `DiffFileViewer` wrapper), so the diff body scrolls while the card
// chrome and the transcript viewport stay put.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import {
  waitForStructuredView,
  enableStructuredViewAndWait,
  attachServeDiagnostics,
} from "../../helpers/acp";

// A single new-string line far wider than a 480px card, with no break
// opportunities, so `whitespace-pre` forces it past the card edge.
const LONG_LINE = `const x = "${"a".repeat(300)}";`;

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "tool_call",
          toolCallId: "tc-edit-1",
          title: "edit big.txt",
          kind: "edit",
          status: "completed",
          rawInput: {
            file_path: "big.txt",
            old_string: "const x = 1;",
            new_string: LONG_LINE,
          },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("edit card diff scrolls horizontally on a narrow viewport", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-edit-scroll-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    // Narrow viewport so the long line is wider than the card.
    await page.setViewportSize({ width: 480, height: 800 });

    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-edit-scroll" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-edit-scroll");
    if (!seeded) throw new Error("seeded session 'story-edit-scroll' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("edit the file");
    await composer.press("Enter");

    // The edit card renders collapsed; its header carries the file path.
    const cardHeader = page
      .getByRole("button")
      .filter({ hasText: "big.txt" })
      .first();
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
      .poll(async () =>
        diff.evaluate(
          (el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth,
        ),
      )
      .toBeGreaterThan(0);

    // Chrome stays put: scrolling lives on the diff body, not the transcript
    // viewport, so the whole panel never gains a horizontal scrollbar.
    const viewport = page.getByTestId("acp-viewport");
    await expect(viewport).toBeVisible();
    await expect
      .poll(async () =>
        viewport.evaluate(
          (el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth,
        ),
      )
      .toBeLessThanOrEqual(0);
  } finally {
    try {
      if (serveHandle) await attachServeDiagnostics(testInfo, serveHandle);
    } catch {
      // best-effort diagnostics; do not block cleanup
    }
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
