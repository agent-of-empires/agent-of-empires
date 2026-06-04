// User story: the mobile terminal toolbar must not render on a
// desktop viewport, even when a session is open.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base("mobile toolbar is hidden on a desktop viewport", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-toolbar-desktop" }),
  });

  try {
    await page.setViewportSize({ width: 1280, height: 720 });
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-toolbar-desktop");
    if (!seeded) throw new Error("seeded session 'story-toolbar-desktop' missing");
    const sessionId = seeded.id;

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await expect(page).toHaveURL(new RegExp(`/session/${sessionId}`), {
      timeout: 10_000,
    });

    await expect(
      page.getByRole("button", { name: "Arrow up" }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "Ctrl+C interrupt" }),
    ).toHaveCount(0);
  } finally {
    await serve.stop();
  }
});
