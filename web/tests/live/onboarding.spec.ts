// First-run surfaces and usage signals: tutorial, tips of the day, and telemetry pings.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect } from "../helpers/liveTest";
import { resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

/** Auto-launching onboarding is suppressed for automated browsers; present as a real one. */
const presentAsRealBrowser = (page: Page) =>
  page.addInitScript(() => Object.defineProperty(navigator, "webdriver", { get: () => false }));

type ApiJson = {
  app_state?: { has_seen_web_tour?: boolean };
  tips?: { id: string; seen?: boolean }[];
  enabled?: boolean;
} | null;
const fetchJson = (page: Page, path: string): Promise<ApiJson> =>
  page.evaluate(async (p) => {
    const res = await fetch(p, { cache: "no-store" });
    return res.ok ? res.json() : null;
  }, path);

const tourSeen = async (page: Page) => (await fetchJson(page, "/api/settings"))?.app_state?.has_seen_web_tour === true;
/** The intro folds the Daemon holds in synced web UI state. */
const storedIntros = (page: Page) =>
  page.evaluate(async () => {
    const res = await fetch("/api/app-state/web-ui-state", { cache: "no-store" });
    const state: Record<string, string> = res.ok ? await res.json() : {};
    return Object.fromEntries(Object.entries(state).filter(([k]) => k.startsWith("aoe-settings-intro-")));
  });

async function openTopBarMenuItem(page: Page, name: string) {
  await page.getByRole("button", { name: "More options" }).click();
  await page.getByRole("menuitem", { name }).click();
}

test("first-run tutorial: auto-launch, skip, persist, re-trigger, and the Settings intros it resets", async ({
  page,
  serve,
}) => {
  // #1513; the seen flag is server-side since #1832.
  const tourStep = (title: string) => page.getByRole("alertdialog", { name: title, exact: true });
  const firstStep = tourStep("Search and commands");
  await presentAsRealBrowser(page);
  // Tips would also auto-pop after reload; keep the tour the only modal here.
  const disableTipsRes = await page.request.post(`${serve.baseUrl}/api/tips/show`, { data: { enabled: false } });
  expect(disableTipsRes.ok(), "failed to disable tips before tutorial isolation").toBeTruthy();

  await page.goto(serve.baseUrl);
  // #1834: the theme welcome shows first.
  await expect(page.getByRole("heading", { name: "Welcome to Agent of Empires" })).toBeVisible({ timeout: 10_000 });
  await page.getByRole("button", { name: "Continue" }).click();

  await expect(firstStep).toBeVisible({ timeout: 10_000 });
  const skip = page.getByRole("button", { name: "Skip tour", exact: true });
  await expect(skip).toBeVisible();
  const postSeen = page.waitForResponse(
    (r) => r.url().includes("/api/app-state/web-tour-seen") && r.request().method() === "POST",
    { timeout: 10_000 },
  );
  await skip.click();
  expect((await postSeen).status()).toBe(200);
  await expect(firstStep).toBeHidden();
  // #2819: a stranded scrim blocks the whole page.
  await expect(page.locator(".react-joyride__overlay")).toHaveCount(0);
  await expect.poll(() => tourSeen(page), { timeout: 20_000 }).toBe(true);

  await page.reload();
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
  await expect(firstStep).toBeHidden();
  await expect(page.getByRole("heading", { name: "Welcome to Agent of Empires" })).toBeHidden();

  // #4594: a Settings tab intro folds into "About" and stays folded across a reload.
  const intro = page.getByRole("region", { name: "About this tab" });
  const about = page.getByRole("button", { name: "About this tab" });
  await page.goto(`${serve.baseUrl}/settings/worktree`);
  await expect(intro).toBeVisible({ timeout: 10_000 });
  const details = intro.getByRole("button", { name: "Details" });
  await details.focus();
  await page.keyboard.press("Enter");
  await expect(details).toHaveAttribute("aria-expanded", "true");
  await expect(intro.getByText(/path template expands/)).toBeVisible();
  await intro.getByRole("button", { name: "Got it" }).click();
  await expect(about).toBeVisible();
  await expect.poll(() => storedIntros(page)).toEqual({ "aoe-settings-intro-worktree": "folded" });
  // Drop this browser's copy so only the Daemon's can fold it again, as on another Device.
  await page.evaluate(() => localStorage.removeItem("aoe-settings-intro-worktree"));
  await page.reload();
  await expect(about).toBeVisible({ timeout: 10_000 });
  await expect(intro).toBeHidden();

  await page.goto(serve.baseUrl);
  await openTopBarMenuItem(page, "Show tutorial");
  await expect(firstStep).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(() => storedIntros(page))
    .toEqual({
      "aoe-settings-intro-worktree": "unfolded",
      "aoe-settings-intro-plugins": "unfolded",
      "aoe-settings-intro-structured-view": "unfolded",
    });

  // #4594: the tour stays on the dashboard.
  const next = page.getByRole("button", { name: /^Next/ });
  for (const heading of ["Your sessions", "Start a session", "Settings", "Replay this tour any time"]) {
    await next.click();
    await expect(tourStep(heading)).toBeVisible({ timeout: 10_000 });
    expect(new URL(page.url()).pathname).toBe("/");
  }
  await page.getByRole("button", { name: /^Done/ }).click();
  await expect(tourStep("Replay this tour any time")).toBeHidden();
  // #2819: the last step must end the tour itself.
  await expect(page.locator(".react-joyride__overlay")).toHaveCount(0);
  expect(await tourSeen(page)).toBe(true);

  // Replaying unfolded the intro; it reads at 375px without sideways scroll.
  await page.setViewportSize({ width: 375, height: 800 });
  await page.goto(`${serve.baseUrl}/settings/worktree`);
  await expect(intro).toBeVisible({ timeout: 10_000 });
  const box = await intro.boundingBox();
  expect(box && box.x >= 0 && box.x + box.width <= 375).toBe(true);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(375);
});

test("a read-only dashboard keeps a Settings intro fold on the device across a reload", async ({
  serveReadOnly,
  page,
}) => {
  const intro = page.getByRole("region", { name: "About this tab" });
  const about = page.getByRole("button", { name: "About this tab" });
  await page.goto(`${serveReadOnly.baseUrl}/settings/worktree`);
  await expect(intro).toBeVisible({ timeout: 10_000 });
  await intro.getByRole("button", { name: "Got it" }).click();
  await expect(about).toBeVisible();
  // The Daemon refuses the write, so the fold lives only in this browser's device key.
  expect(await storedIntros(page)).toEqual({});
  expect(await page.evaluate(() => localStorage.getItem("aoe-local-settings-intro-worktree"))).toBe("folded");
  await page.reload();
  await expect(about).toBeVisible({ timeout: 10_000 });
  await expect(intro).toBeHidden();
});

// #2292: tip of the day, shared seen state with the TUI.
test.describe("tips", () => {
  const PWA_TIP = "Install the dashboard as an app";

  test("tips: auto-pops on startup and marks the shown tip seen", async ({ page, serve }) => {
    await presentAsRealBrowser(page);
    // Clear the earlier onboarding phases: theme welcome (localStorage) and tour (server).
    await page.addInitScript(() => window.localStorage.setItem("aoe-welcome-seen", "1"));
    await page.request.post(`${serve.baseUrl}/api/app-state/web-tour-seen`);
    const postSeen = page.waitForResponse(
      (r) => r.url().includes("/api/app-state/tip-seen") && r.request().method() === "POST",
      { timeout: 15_000 },
    );
    await page.goto(serve.baseUrl);

    await expect(page.getByRole("heading", { name: "Tip of the day" })).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("heading", { name: PWA_TIP })).toBeVisible();
    // The TUI-only tip never appears on the web.
    await expect(page.getByText("Reuse the selected session's settings")).toBeHidden();
    await expect(page.getByText(/Tip 1 of \d+/)).toBeVisible();

    expect((await postSeen).status()).toBe(200);
    await page.getByRole("button", { name: "Close" }).click();
    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    // The POST returns before config.toml is flushed.
    await expect
      .poll(
        () =>
          fetchJson(page, "/api/tips").then(
            (data) => data?.tips?.find((t) => t.id === "install-dashboard-pwa")?.seen === true,
          ),
        { timeout: 10_000 },
      )
      .toBe(true);
  });

  test("tips: reopen from the menu and persist the startup toggle", async ({ page, serve }) => {
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    await openTopBarMenuItem(page, "Tips");
    await expect(page.getByRole("heading", { name: "Tip of the day" })).toBeVisible();
    const checkbox = page.getByRole("checkbox", { name: "Show tips on startup" });
    await expect(checkbox).toBeChecked();

    const postShow = page.waitForResponse(
      (r) => r.url().includes("/api/tips/show") && r.request().method() === "POST",
      { timeout: 10_000 },
    );
    await checkbox.uncheck();
    expect((await postShow).status()).toBe(200);
    await page.getByRole("button", { name: "Close" }).click();

    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    const tips = await fetchJson(page, "/api/tips");
    expect(tips === null ? true : tips.enabled).toBe(false);
    await openTopBarMenuItem(page, "Tips");
    await expect(page.getByRole("checkbox", { name: "Show tips on startup" })).not.toBeChecked();
  });
});

test.describe("telemetry signals", () => {
  type Ping = { surface?: string; form_factor?: string };
  /** Record `POST /api/telemetry/seen` bodies the page sends. */
  function captureSeenPings(page: Page): () => Ping[] {
    const pings: Ping[] = [];
    page.on("request", (req) => {
      const body = req.postData();
      if (req.method() === "POST" && req.url().includes("/api/telemetry/seen") && body) pings.push(JSON.parse(body));
    });
    return () => pings;
  }

  test("desktop and mobile-PWA clients report distinct form-factor classes", async ({ browser, serve }) => {
    // #1883. The mobile context forces the classifier's media queries instead of relying on emulation.
    const desktopCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const mobileCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: true,
      isMobile: true,
    });
    await mobileCtx.addInitScript(() => {
      const orig = window.matchMedia.bind(window);
      const forced: Record<string, boolean> = {
        "(display-mode: standalone)": true,
        "(pointer: coarse)": true,
        "(min-width: 768px)": false,
      };
      window.matchMedia = ((query: string) =>
        query in forced
          ? ({
              matches: forced[query],
              media: query,
              onchange: null,
              addListener: () => {},
              removeListener: () => {},
              addEventListener: () => {},
              removeEventListener: () => {},
              dispatchEvent: () => false,
            } as unknown as MediaQueryList)
          : orig(query)) as typeof window.matchMedia;
    });
    try {
      const desktopPage = await desktopCtx.newPage();
      const desktopPings = captureSeenPings(desktopPage);
      await desktopPage.goto(serve.baseUrl);
      const mobilePage = await mobileCtx.newPage();
      const mobilePings = captureSeenPings(mobilePage);
      await mobilePage.goto(serve.baseUrl);

      const hasPing = (pings: () => Ping[], formFactor: string) =>
        expect
          .poll(() => pings().some((p) => p.surface === "web" && p.form_factor === formFactor), {
            timeout: 10_000,
          })
          .toBe(true);
      await hasPing(desktopPings, "desktop");
      await hasPing(mobilePings, "mobile_pwa");
      expect(desktopPings().some((p) => p.form_factor === "mobile_pwa")).toBe(false);
    } finally {
      await desktopCtx.close();
      await mobileCtx.close();
    }
  });

  test("opening a session fires the diff_panel and web_terminal signals", async ({ page, spawnServe }) => {
    // #1881
    const serve = await spawnServe({
      seedFn: ({ home, env }) => {
        const projectDir = initWorkingRepo(join(home, "project"), env).path;
        writeFiles(projectDir, { "src/a.ts": "export const a = 1;\n" });
        commitAll(projectDir, "baseline", env);
        // An uncommitted edit gives the diff panel something to show.
        writeFiles(projectDir, { "src/a.ts": "export const a = 11;\n" });
        const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "usage-signals", "-c", "claude"], {
          env,
        });
        if (addRes.status !== 0) {
          throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
        }
      },
    });
    const pings = captureSeenPings(page);
    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "usage-signals" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();
    for (const surface of ["web_terminal", "diff_panel"]) {
      await expect.poll(() => pings().some((p) => p.surface === surface), { timeout: 10_000 }).toBe(true);
    }
  });

  test("a read-only server fires no feature-usage signals", async ({ serveReadOnly, page }) => {
    const pings = captureSeenPings(page);
    const about = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, {
      timeout: 10_000,
    });
    await page.goto(serveReadOnly.baseUrl);
    await about;
    await expect(page.getByText("This dashboard is in read-only mode.")).toBeVisible();
    expect(pings()).toHaveLength(0);
  });
});
