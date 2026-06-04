// User story (#1469): long unbreakable tokens in structured view messages wrap
// inside the chat bubble instead of forcing a viewport-level horizontal
// scrollbar.
//
// On a narrow viewport an agent message containing an 80+ char autolinked
// URL, a long absolute file path, and a `─` rule line (the shape Playwright's
// list reporter emits) used to push its bubble past `max-w-[80%]`; the chat
// viewport's `overflow-y-auto` then resolved `overflow-x` to `auto` and the
// whole transcript gained a horizontal scrollbar.
//
// Fix lives in two places:
//   - `.acp-markdown :where(p, li, blockquote, a)` gets
//     `overflow-wrap: anywhere` (web/src/index.css) so prose tokens wrap and
//     the bubble reports a small min-content width.
//   - the chat viewport gets `overflow-x-hidden` (StructuredView.tsx) as a
//     belt-and-suspenders clamp.
//
// Fenced code blocks keep their own `overflow-x-auto` and must still scroll
// internally without growing the viewport.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect, type Locator } from "@playwright/test";
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

const LONG_URL =
  "https://github.com/njbrake/agent-of-empires/actions/runs/26342421371/job/77546632641";

// A prose line carrying an unbreakable absolute path token and a run of `─`
// rule characters. Kept un-indented on purpose so markdown renders it as a
// paragraph (the surface the fix targets), not an indented code block.
const PW_PROSE =
  "Failure at /Users/seluj78/aoe/agent-of-empires-worktrees/fix-flaky-pw-tests/web/tests/terminal-focus-shortcut.spec.ts:79:48 ────────────────────────────────────";

// A fenced code block whose single line is far wider than the bubble. The
// code container owns its own horizontal scroll; the viewport must not.
const LONG_CODE_LINE = "const x = " + "a".repeat(200) + ";";

const OVERFLOW_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: {
            type: "text",
            text:
              `Run link: ${LONG_URL}\n\n` +
              `${PW_PROSE}\n\n` +
              "```ts\n" +
              `${LONG_CODE_LINE}\n` +
              "```\n",
          },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("long URL, PW paste, and code line stay inside the chat viewport", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-overflow-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(OVERFLOW_SCRIPT));

  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;

  try {
    // Narrow viewport so the unbreakable tokens are wider than the bubble.
    await page.setViewportSize({ width: 480, height: 800 });

    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-overflow" }),
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-overflow");
    if (!seeded) throw new Error("seeded session 'story-overflow' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("show me the failure");
    await composer.press("Enter");

    // Wait for the agent message (the autolinked URL) to render.
    const link = page.getByRole("link", { name: LONG_URL });
    await expect(link).toBeVisible({ timeout: 10_000 });

    const viewport = page.getByTestId("acp-viewport");
    await expect(viewport).toBeVisible();

    // Core regression: the wrapped content fits, so the viewport never grows
    // a horizontal scroll area. (Pre-fix, the URL/path do not wrap and
    // scrollWidth exceeds clientWidth.)
    await expect
      .poll(async () =>
        viewport.evaluate(
          (el) => (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth,
        ),
      )
      .toBeLessThanOrEqual(0);

    // Belt-and-suspenders clamp is in place.
    await expect(viewport).toHaveCSS("overflow-x", "hidden");

    // Fenced code block keeps its own horizontal-scroll affordance: the
    // scroll container's computed overflow-x is auto/scroll (the wrap rule
    // targets p/li/blockquote/a only, so the code container is untouched).
    const codeScroller: Locator = viewport
      .locator(".acp-markdown .overflow-x-auto")
      .first();
    await expect(codeScroller).toBeVisible();
    const codeOverflowX = await codeScroller.evaluate(
      (el) => getComputedStyle(el).overflowX,
    );
    expect(["auto", "scroll"]).toContain(codeOverflowX);

    // The wrap rule must NOT leak into code: the long line stays a single
    // unwrapped line, so the code <pre>'s content is wider than its box.
    // (scrollWidth reports the full content width even though the bubble's
    // `pre { overflow: hidden }` clips it.) If overflow-wrap leaked here the
    // line would wrap and scrollWidth would collapse to clientWidth.
    const codePre: Locator = codeScroller.locator("pre").first();
    const codeLineUnwrapped = await codePre.evaluate(
      (el) => (el as HTMLElement).scrollWidth > (el as HTMLElement).clientWidth,
    );
    expect(codeLineUnwrapped).toBe(true);
  } finally {
    if (serve) {
      await attachServeDiagnostics(testInfo, serve);
      await serve.stop();
    }
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
