// A Session paused by the Agent's usage limit: the banner above the composer at phone width.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

const agent = (name: string) => ({
  name,
  kind: "builtin",
  binary: name,
  host_only: false,
  installed: true,
  install_hint: "",
  acp_capable: true,
  acp_installed: true,
  display_name: { claude: "Claude Code", codex: "Codex" }[name] ?? name,
});

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the usage-limit banner fits above the composer and hands off to another Agent", async ({ page }) => {
    const resetsAt = new Date(Date.now() + 2 * 3600_000 + 30_000).toISOString();
    const mock = await mockAcpSession(page, {
      title: "story-usage-limit",
      rateLimitAutoResume: false,
      reducedState: { rate_limit: { status: "limited", resets_at: resetsAt, kind: "usage" } },
    });
    await page.route("**/api/agents", (r) => r.fulfill({ json: [agent("claude"), agent("codex")] }));
    await page.route("**/api/acp/agents", (r) =>
      r.fulfill({
        json: [
          { name: "claude", description: "Claude", command: "claude-agent-acp" },
          { name: "codex", description: "OpenAI Codex", command: "codex-acp" },
        ],
      }),
    );
    await openStructuredSession(page, mock);

    const banner = page.getByTestId("usage-limit-banner");
    await expect(banner).toContainText("Claude Code usage limit reached");
    await expect(banner).toContainText(/Resets .+ · in about 2h/);
    await expect(banner.getByRole("link", { name: "Turn on" })).toHaveAttribute(
      "href",
      "/settings/structured-view?profile=default&field=acp.rate_limit_auto_resume",
    );
    const switchAgent = banner.getByRole("button", { name: "Switch Agent" });
    await expect(switchAgent).toBeInViewport();
    await expect(banner.getByTestId("usage-limit-resume")).toHaveText("Try again");
    await expect(banner.getByTestId("usage-limit-resume")).toBeInViewport();

    const bannerBox = (await banner.boundingBox())!;
    const footerBox = (await page.getByTestId("composer-footer").boundingBox())!;
    expect(bannerBox.y + bannerBox.height).toBeLessThanOrEqual(footerBox.y);
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);

    await switchAgent.click();
    const dialog = page.getByRole("dialog", { name: "Continue in another Agent" });
    await expect(dialog.getByText("current · limit reached")).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Continue in Codex" })).toBeVisible();
  });
});

test("a limited Session switches account in the background and toasts a refusal", async ({ page }) => {
  const resetsAt = new Date(Date.now() + 2 * 3600_000 + 30_000).toISOString();
  const mock = await mockAcpSession(page, {
    title: "story-switch-account",
    rateLimitAutoResume: false,
    reducedState: {
      rate_limit: { status: "limited", resets_at: resetsAt, kind: "usage" },
      auth_status: { kind: "account", label: "Claude Max", account: { email: "me@example.com", plan: "Max" } },
    },
  });
  await page.route("**/api/agents", (r) => r.fulfill({ json: [agent("claude")] }));
  await page.route("**/api/sessions/*/account-targets", (r) => r.fulfill({ json: { targets: ["claude-b"] } }));
  const restarts: unknown[] = [];
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route("**/api/sessions/*/restart", async (r) => {
    restarts.push(r.request().postDataJSON());
    await held;
    await r.fulfill({
      status: 409,
      json: { error: "account_swap_unavailable", message: "Cannot switch to 'claude-b'. Nothing changed." },
    });
  });
  await openStructuredSession(page, mock);

  await page.getByTestId("usage-limit-switch-account").click();
  const dialog = page.getByTestId("switch-account-modal");
  await expect(dialog).toContainText("copied into the other account's store");
  await expect(dialog).toContainText("uncached on the new account");
  await expect(dialog).toContainText("Claude Max");
  await expect(dialog).not.toContainText("me@example.com");

  await dialog.getByTestId("switch-account-confirm").click();

  // The restart is still in flight: no overlay, an inline status, and the page takes input.
  await expect(dialog).toBeHidden();
  await expect(page.getByTestId("usage-limit-failover")).toHaveText("Switching to claude-b…");
  await expect(page.getByTestId("usage-limit-switch-account")).toBeHidden();
  const composer = page.getByRole("textbox").last();
  await composer.fill("still typing");
  await expect(composer).toHaveValue("still typing");

  release();
  await expect(page.getByTestId("toast-list")).toContainText("Cannot switch to 'claude-b'. Nothing changed.");
  await expect(page.getByTestId("usage-limit-failover")).toBeHidden();
  await expect(page.getByTestId("usage-limit-switch-account")).toBeVisible();
  expect(restarts).toEqual([{ tool: "claude-b", require_account_carry: true }]);
});

test("a limited Session switches account in the background and the status clears on success", async ({ page }) => {
  const resetsAt = new Date(Date.now() + 2 * 3600_000 + 30_000).toISOString();
  const mock = await mockAcpSession(page, {
    title: "story-switch-account-ok",
    rateLimitAutoResume: false,
    reducedState: { rate_limit: { status: "limited", resets_at: resetsAt, kind: "usage" } },
  });
  await page.route("**/api/agents", (r) => r.fulfill({ json: [agent("claude")] }));
  await page.route("**/api/sessions/*/account-targets", (r) => r.fulfill({ json: { targets: ["claude-b"] } }));
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route("**/api/sessions/*/restart", async (r) => {
    await held;
    await r.fulfill({ json: {} });
  });
  await openStructuredSession(page, mock);

  await page.getByTestId("usage-limit-switch-account").click();
  await page.getByTestId("switch-account-confirm").click();
  await expect(page.getByTestId("switch-account-modal")).toBeHidden();
  await expect(page.getByTestId("usage-limit-failover")).toHaveText("Switching to claude-b…");

  release();
  await expect(page.getByTestId("usage-limit-failover")).toBeHidden();
  await expect(page.getByTestId("toast-list")).toBeHidden();
});

test("the switch-account dialog keeps Tab inside it with a non-first account selected", async ({ page }) => {
  const resetsAt = new Date(Date.now() + 2 * 3600_000 + 30_000).toISOString();
  const mock = await mockAcpSession(page, {
    title: "story-switch-account-focus",
    rateLimitAutoResume: false,
    reducedState: { rate_limit: { status: "limited", resets_at: resetsAt, kind: "usage" } },
  });
  await page.route("**/api/agents", (r) => r.fulfill({ json: [agent("claude")] }));
  await page.route("**/api/sessions/*/account-targets", (r) =>
    r.fulfill({ json: { targets: ["claude-b", "claude-c"] } }),
  );
  await openStructuredSession(page, mock);

  await page.getByTestId("usage-limit-switch-account").click();
  const dialog = page.getByTestId("switch-account-modal");
  const second = dialog.getByLabel("claude-c");
  await second.check();
  await expect(second).toBeFocused();

  // The checked radio is the group's only tab stop, so Shift+Tab from it must wrap, not leave.
  await page.keyboard.press("Shift+Tab");
  await expect(dialog.getByTestId("switch-account-confirm")).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(second).toBeFocused();
});

test("Turn on opens the Session profile's auto-resume row and highlights it", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-usage-limit-setting",
    rateLimitAutoResume: false,
    reducedState: {
      rate_limit: { status: "limited", resets_at: null, kind: "usage" },
    },
  });
  await page.route("**/api/agents", (r) => r.fulfill({ json: [agent("claude")] }));
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) =>
      r.fulfill({
        json: [
          {
            section: "acp",
            field: "rate_limit_auto_resume",
            label: "Auto-resume after rate limit",
            category: "acp",
            description: "",
            widget: { kind: "toggle" },
            advanced: false,
            profile_overridable: true,
            validation: { rule: "none" },
            web_write: { policy: "allow" },
          },
        ],
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings" || /^\/api\/profiles\/[^/]+\/settings$/.test(url.pathname),
    (r) => r.fulfill({ json: { acp: { rate_limit_auto_resume: false } } }),
  );
  await openStructuredSession(page, mock);

  await page.getByTestId("usage-limit-banner").getByRole("link", { name: "Turn on" }).click();
  await expect(page).toHaveURL(/\/settings\/structured-view\?profile=default&field=acp\.rate_limit_auto_resume$/);
  const row = page.locator('[data-settings-field="acp.rate_limit_auto_resume"]');
  await expect(row).toBeVisible();
  await expect(row).toHaveClass(/animate-settings-highlight/);
});
