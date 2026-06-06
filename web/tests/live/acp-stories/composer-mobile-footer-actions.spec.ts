// User story (#1717): on a narrow mobile viewport the composer footer
// keeps the right-side action button reachable even when the left
// control cluster (mode + model + effort + attachment) is wide.
//
// Pre-fix the footer was a single non-wrapping flex row with
// `justify-between` and no shrink budget, so the populated left cluster
// pushed the Send / Stop cluster past the clipped viewport edge and the
// action button became untappable. The fix lets the left cluster wrap
// onto extra rows and pins the right cluster with `shrink-0`.
//
// The fake ACP agent advertises model + reasoning-effort config options
// by default, so simply enabling structured view on a narrow viewport recreates
// the worst-case left-cluster width. The fix is pure responsive CSS
// (no pointer-capability branch), so a narrow viewport alone reproduces
// it; no coarse-pointer emulation is needed.

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

base(
  "mobile composer footer keeps the Send action reachable when config controls are present",
  async ({ page }, testInfo) => {
    let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;

    try {
      // Narrow viewport: the populated left cluster is wider than the row.
      await page.setViewportSize({ width: 360, height: 740 });

      serve = await spawnAoeServe({
        authMode: "none",
        acp: true,
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: seedSessionViaAoeAdd({ title: "story-footer-actions" }),
      });

      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-footer-actions");
      if (!seeded)
        throw new Error("seeded session 'story-footer-actions' missing");
      const sessionId = seeded.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await page.goto(
        `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
      );
      await waitForStructuredView(page);

      // The model chip rendering confirms the left cluster carries the
      // config controls that create the width pressure this story guards.
      await expect(page.getByTestId("config-option-model")).toBeVisible({
        timeout: 15_000,
      });

      // Core regression: the footer must not overflow horizontally, so the
      // right action cluster is never pushed past the clipped viewport edge.
      const footer = page.getByTestId("composer-footer");
      await expect(footer).toBeVisible();
      await expect
        .poll(async () =>
          footer.evaluate(
            (el) =>
              (el as HTMLElement).scrollWidth - (el as HTMLElement).clientWidth,
          ),
        )
        .toBeLessThanOrEqual(0);

      // The Send button sits entirely within the viewport (pre-fix its
      // right edge exceeded the 360px viewport width).
      const send = page.getByRole("button", { name: "Send message" });
      await expect(send).toBeVisible();
      const box = await send.boundingBox();
      expect(box).not.toBeNull();
      const viewport = page.viewportSize();
      expect(viewport).not.toBeNull();
      expect(box!.x).toBeGreaterThanOrEqual(0);
      expect(box!.x + box!.width).toBeLessThanOrEqual(viewport!.width);

      // And it is actually tappable without a forced click.
      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.fill("reachable on mobile");
      await send.click();
    } finally {
      if (serve) {
        await attachServeDiagnostics(testInfo, serve);
        await serve.stop();
      }
    }
  },
);
