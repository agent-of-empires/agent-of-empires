// User story: the Ctrl toggle on the mobile terminal toolbar latches
// the modifier so the next keystroke combines with Ctrl.
//
// Tapping Ctrl flips aria-pressed to "true"; tapping again flips it
// back. This story covers the latch UI only; the modifier-applied
// keystroke is handled by the terminal helper textarea and is
// exercised separately by the Ctrl+C interrupt story.

import { test as base, expect, devices } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base.use({ ...devices["iPhone 13"] });

base("mobile toolbar Ctrl button latches and unlatches", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-mobile-ctrl" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-mobile-ctrl");
    if (!seeded) throw new Error("seeded session 'story-mobile-ctrl' missing");
    const sessionId = seeded.id;

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    const ctrl = page.getByRole("button", { name: "Ctrl", exact: true });
    await expect(ctrl).toBeVisible({ timeout: 15_000 });
    await expect(ctrl).toHaveAttribute("aria-pressed", "false");

    await ctrl.click();
    await expect(ctrl).toHaveAttribute("aria-pressed", "true");

    await ctrl.click();
    await expect(ctrl).toHaveAttribute("aria-pressed", "false");
  } finally {
    await serve.stop();
  }
});
