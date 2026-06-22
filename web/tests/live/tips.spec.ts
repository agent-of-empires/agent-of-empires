// User stories (issue #2292): the web dashboard mirrors the TUI tips system as a
// GIMP/DBeaver-style tip-of-the-day. A fresh server has one web-eligible tip
// ("Install the dashboard as an app"). The modal auto-pops on startup (once
// onboarding settles), is reopenable from the top-bar menu, marks tips seen as
// they are shown (persisted, shared with the TUI), and the "Show tips on
// startup" checkbox persists via the dedicated endpoint. The TUI-only shortcut
// tip never appears.
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

const PWA_TIP = "Install the dashboard as an app";
const TUI_TIP = "Reuse the selected session's settings";

base("tips: auto-pops on startup and marks the shown tip seen", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    // Auto-pop is suppressed in automated sessions (navigator.webdriver) so the
    // modal never intercepts the rest of the suite; present as a real browser to
    // exercise it, and clear the other onboarding phases so tips pops cleanly:
    // mark the theme welcome seen (localStorage) and the tour seen (server).
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "webdriver", { get: () => false });
      window.localStorage.setItem("aoe-welcome-seen", "1");
    });
    await page.request.post(`${serve.baseUrl}/api/app-state/web-tour-seen`);

    const postSeen = page.waitForResponse(
      (r) => r.url().includes("/api/app-state/tip-seen") && r.request().method() === "POST",
      { timeout: 15_000 },
    );
    await page.goto(serve.baseUrl);

    // Story 1: the tip-of-the-day modal auto-pops on the first unseen tip and
    // never shows the TUI-only one. The PWA tip is the first web tip in the
    // catalog, so it leads.
    await expect(page.getByRole("heading", { name: "Tip of the day" })).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("heading", { name: PWA_TIP })).toBeVisible();
    await expect(page.getByText(TUI_TIP)).toBeHidden();
    // Several web tips ship, so the carousel counter and navigation are present.
    await expect(page.getByText(/Tip 1 of \d+/)).toBeVisible();

    // Story 2: showing a tip marks it seen on the server, and a reload reads
    // that back (the PWA tip no longer leads).
    expect((await postSeen).status()).toBe(200);
    await page.getByRole("button", { name: "Close" }).click();
    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    // The mark-seen POST returns 200 before the flag is flushed to config.toml
    // (same as the tour-seen write), so poll rather than assert once.
    await expect
      .poll(
        () =>
          page.evaluate(async () => {
            const res = await fetch("/api/tips", { cache: "no-store" });
            if (!res.ok) return false;
            const data = await res.json();
            return data.tips.find((t: { id: string }) => t.id === "install-dashboard-pwa")?.seen === true;
          }),
        { timeout: 10_000 },
      )
      .toBe(true);
  } finally {
    await serve.stop();
  }
});

base("tips: reopen from the menu and persist the startup toggle", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    // Default automated session: no auto-pop, so this drives the menu path.
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });

    // Story 3: reopen tips from the top-bar overflow menu.
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Tips" }).click();
    await expect(page.getByRole("heading", { name: "Tip of the day" })).toBeVisible();
    const checkbox = page.getByRole("checkbox", { name: "Show tips on startup" });
    await expect(checkbox).toBeChecked();

    // Story 4: unchecking "Show tips on startup" persists through the dedicated
    // endpoint and survives a reload.
    const postShow = page.waitForResponse(
      (r) => r.url().includes("/api/tips/show") && r.request().method() === "POST",
      { timeout: 10_000 },
    );
    await checkbox.uncheck();
    expect((await postShow).status()).toBe(200);
    await page.getByRole("button", { name: "Close" }).click();

    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    expect(
      await page.evaluate(async () => {
        const res = await fetch("/api/tips", { cache: "no-store" });
        if (!res.ok) return true;
        return (await res.json())?.enabled;
      }),
    ).toBe(false);

    // Reopening from the menu shows the checkbox unchecked.
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Tips" }).click();
    await expect(page.getByRole("checkbox", { name: "Show tips on startup" })).not.toBeChecked();
  } finally {
    await serve.stop();
  }
});
