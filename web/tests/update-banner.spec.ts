import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

const DISMISS_KEY = "aoe-update-dismissed-version";

interface UpdateStatusFixture {
  update_check_mode: "auto" | "notify" | "off";
  current_version: string;
  latest_version: string | null;
  update_available: boolean;
  release_url: string | null;
  web_poll_interval_minutes: number;
  error: string | null;
}

async function mock(page: Page, status: UpdateStatusFixture) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
  await page.route("**/api/system/update-status", (r) => r.fulfill({ json: status }));
}

test.describe("Update banner (#984, #1140)", () => {
  test("renders when update_available is true and mode is notify", async ({ page }) => {
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.5.0",
      latest_version: "0.6.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      web_poll_interval_minutes: 60,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    const banner = page.getByRole("status", { name: /Update available/i });
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("v0.5.0");
    await expect(banner).toContainText("v0.6.0");
    await expect(banner.getByRole("link", { name: "Release notes" })).toHaveAttribute(
      "href",
      "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
    );
  });

  test("hidden when update_check_mode is off (server suppresses)", async ({ page }) => {
    await mock(page, {
      update_check_mode: "off",
      current_version: "0.5.0",
      latest_version: null,
      update_available: false,
      release_url: null,
      web_poll_interval_minutes: 60,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("hidden when update_check_mode is auto (background install)", async ({ page }) => {
    await mock(page, {
      update_check_mode: "auto",
      current_version: "0.5.0",
      latest_version: "0.6.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      web_poll_interval_minutes: 60,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("hidden when latest matches current (no update)", async ({ page }) => {
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.6.0",
      latest_version: "0.6.0",
      update_available: false,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      web_poll_interval_minutes: 60,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("dismiss persists per-version across reload", async ({ page }) => {
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.5.0",
      latest_version: "0.6.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      web_poll_interval_minutes: 60,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    const banner = page.getByRole("status", { name: /Update available/i });
    await expect(banner).toBeVisible();
    await page.getByRole("button", { name: /Dismiss update notice/i }).click();
    await expect(banner).toHaveCount(0);
    expect(await page.evaluate((k) => localStorage.getItem(k), DISMISS_KEY)).toBe("0.6.0");

    await page.reload();
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("dismissed version no longer suppresses newer release", async ({ page }) => {
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.5.0",
      latest_version: "0.7.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.7.0",
      web_poll_interval_minutes: 60,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    // Seed an older dismissed version; banner must still appear.
    await page.addInitScript((k) => localStorage.setItem(k, "0.6.0"), DISMISS_KEY);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toBeVisible();
  });
});
