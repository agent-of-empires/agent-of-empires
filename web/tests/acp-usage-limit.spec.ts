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
      "/settings/structured-view?profile=default",
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
