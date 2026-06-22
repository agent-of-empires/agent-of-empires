// User stories (issue #2292): the web dashboard mirrors the TUI tips system.
// A fresh server has one web-eligible rotation tip ("Install the dashboard as
// an app"), so the 💡 badge shows in the TopBar. Opening the panel and reading
// a tip marks it seen (persisted via POST /api/app-state/tip-seen, shared with
// the TUI), and "Don't show again" turns tips off (POST /api/tips/disable). Both
// survive a reload via GET /api/tips. The TUI-only shortcut tip never appears.
import { test as base, expect, type Page } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

const PWA_TIP = "Install the dashboard as an app";

// A fresh $HOME shows the theme welcome modal first; dismiss it so the TopBar
// badge is clickable. The tour does not auto-launch here (Playwright presents
// navigator.webdriver = true, which suppresses it).
async function dismissWelcome(page: Page) {
  const welcome = page.getByText("Choose your theme");
  if (await welcome.isVisible().catch(() => false)) {
    await page.getByRole("button", { name: "Continue" }).click();
  }
}

base("tips: badge surfaces a tip, reading it marks it seen across reloads", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    await dismissWelcome(page);

    // Story 1: the badge shows for the single unseen web tip.
    const badge = page.getByRole("button", { name: "1 tip" });
    await expect(badge).toBeVisible({ timeout: 10_000 });

    // Story 2: opening the panel lists the web tip (and never the TUI-only one).
    await badge.click();
    await expect(page.getByRole("heading", { name: "Tips" })).toBeVisible();
    await expect(page.getByText(PWA_TIP)).toBeVisible();
    await expect(page.getByText("Reuse the selected session's settings")).toBeHidden();

    // Story 3: reading (expanding) an unseen tip marks it seen on the server.
    const postSeen = page.waitForResponse(
      (r) => r.url().includes("/api/app-state/tip-seen") && r.request().method() === "POST",
      { timeout: 10_000 },
    );
    await page.getByRole("button", { name: new RegExp(PWA_TIP) }).click();
    expect((await postSeen).status()).toBe(200);
    await page.getByRole("button", { name: "Close" }).click();

    // Badge clears once the only tip is seen, and stays gone after a reload
    // (seen state is server-side, so a new page load reads it back).
    await expect(page.getByRole("button", { name: /tips?$/ })).toHaveCount(0);
    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("button", { name: /tips?$/ })).toHaveCount(0);
  } finally {
    await serve.stop();
  }
});

base("tips: Don't show again turns tips off across reloads", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    await dismissWelcome(page);

    const badge = page.getByRole("button", { name: "1 tip" });
    await expect(badge).toBeVisible({ timeout: 10_000 });
    await badge.click();

    const postDisable = page.waitForResponse(
      (r) => r.url().includes("/api/tips/disable") && r.request().method() === "POST",
      { timeout: 10_000 },
    );
    await page.getByRole("button", { name: "Don't show again" }).click();
    expect((await postDisable).status()).toBe(200);

    // Badge gone immediately and after a reload; GET /api/tips reports disabled.
    await expect(page.getByRole("button", { name: /tips?$/ })).toHaveCount(0);
    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("button", { name: /tips?$/ })).toHaveCount(0);
    expect(
      await page.evaluate(async () => {
        const res = await fetch("/api/tips", { cache: "no-store" });
        if (!res.ok) return true;
        return (await res.json())?.enabled;
      }),
    ).toBe(false);
  } finally {
    await serve.stop();
  }
});
