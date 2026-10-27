// #2144: a long transcript renders its recent slice first; 100 turns (200 rows) exceed the 150-row window.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import {
  mockAcpSession,
  openStructuredSession,
  agentMessageChunk,
  stopped,
  waitForComposerConnected,
} from "./helpers/acpMock";

function userPrompt(text: string) {
  return { UserPromptSent: { text } };
}

const TURNS = 100;

function longTranscript(turns = TURNS): unknown[] {
  const events: unknown[] = [];
  for (let i = 0; i < turns; i += 1) {
    events.push(userPrompt(`prompt number ${i}`));
    events.push(agentMessageChunk(`reply number ${i}`));
    events.push(stopped());
  }
  return events;
}

// The button remounts while a `before` fetch is in flight, so re-resolve it on every poll (#2236 flake).
async function revealOldestTurn(page: Page): Promise<void> {
  const oldest = page.getByText("prompt number 0");
  await expect(async () => {
    if ((await oldest.count()) === 0) {
      await page
        .getByTestId("acp-load-earlier")
        .click({ timeout: 2_000 })
        .catch(() => {});
    }
    expect(await oldest.count()).toBeGreaterThan(0);
  }).toPass({ timeout: 30_000 });
}

// #2236: scrolling to the top loads earlier messages without a click.
test("scrolling to the top auto-loads earlier messages", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-history-autoload",
    initialEvents: longTranscript(),
  });
  await openStructuredSession(page, mock);

  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  await page.getByTestId("acp-viewport").hover();
  await page.mouse.wheel(0, -100_000);
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

// #2236: history beyond one replay page is fetched from the server with `before`.
test("loads older events from the server when the loaded window is exhausted", async ({ page }) => {
  // 1050 events exceed the client's 1000-event page.
  const events: unknown[] = [];
  for (let i = 0; i < 350; i += 1) {
    events.push(userPrompt(`prompt number ${i}`));
    events.push(agentMessageChunk(`reply number ${i}`));
    events.push(stopped());
  }
  const mock = await mockAcpSession(page, { title: "story-history-network", initialEvents: events });
  await openStructuredSession(page, mock);

  await expect(page.getByText("reply number 349")).toBeVisible({ timeout: 10_000 });
  await waitForComposerConnected(page);
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  const olderPage = page.waitForResponse((response) => {
    const url = new URL(response.url());
    const before = Number(url.searchParams.get("before"));
    return (
      url.pathname.endsWith(`/sessions/${encodeURIComponent(mock.sessionId)}/acp/replay`) &&
      url.searchParams.get("view") === "rows" &&
      Number.isFinite(before) &&
      before > 0 &&
      before < Number.MAX_SAFE_INTEGER
    );
  });
  const [response] = await Promise.all([olderPage, revealOldestTurn(page)]);
  expect(response.status()).toBe(200);
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

test("persists the initial history count through synced settings and keeps the reveal step fixed", async ({ page }) => {
  // Register before the ACP helper so its populated session list takes precedence.
  await mockSettingsApis(page);
  const mock = await mockAcpSession(page, {
    title: "story-history-preference",
    initialEvents: longTranscript(320),
  });
  // Settings fetches the selected profile through a query parameter.
  await page.route(
    (url) => url.pathname === "/api/settings",
    (route) => route.fulfill({ json: {} }),
  );
  const synced: Record<string, string> = {};
  await page.route("**/api/app-state/web-ui-state", async (route) => {
    if (route.request().method() === "PATCH") {
      const patch = route.request().postDataJSON() as Record<string, string | null>;
      for (const [key, value] of Object.entries(patch)) {
        if (value === null) delete synced[key];
        else synced[key] = value;
      }
    }
    await route.fulfill({ json: synced });
  });

  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  await expect(page.getByText("reply number 319", { exact: true })).toBeVisible();
  await expect(page.getByText("prompt number 244", { exact: true })).toHaveCount(0);
  await expect(page.getByText("prompt number 245", { exact: true })).toHaveCount(1);

  await page.goto("/settings/structured-view");
  const history = page.getByRole("spinbutton", { name: "Initial visible history" });
  await expect(history).toHaveValue("150");
  await history.fill("200");
  await history.press("Enter");
  await expect.poll(() => JSON.parse(synced["aoe-web-settings"] ?? "{}").structuredHistoryWindow).toBe(200);

  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  await expect(page.getByText("reply number 319", { exact: true })).toBeVisible();
  const prompts = page
    .getByTestId("acp-viewport")
    .locator("p")
    .filter({ hasText: /^prompt number \d+$/ });
  await expect(prompts).toHaveCount(100);
  await expect(page.getByText("prompt number 219", { exact: true })).toHaveCount(0);
  await expect(page.getByText("prompt number 220", { exact: true })).toHaveCount(1);
  // Avoid scrolling the button into view, which separately triggers scroll-up autoload.
  await page.getByTestId("acp-load-earlier").evaluate((element) => (element as HTMLButtonElement).click());
  await expect(prompts).toHaveCount(175);
  await expect(page.getByText("prompt number 145", { exact: true })).toHaveCount(1);

  await page.goto("/settings/structured-view");
  await history.fill("500");
  await history.blur();
  await expect.poll(() => JSON.parse(synced["aoe-web-settings"] ?? "{}").structuredHistoryWindow).toBe(500);
  await page.evaluate(() => localStorage.removeItem("aoe-web-settings"));
  await page.reload();
  await expect(history).toHaveValue("500");

  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  await expect(page.getByText("reply number 319", { exact: true })).toBeVisible();
  await expect(prompts).toHaveCount(250);
  await expect(page.getByText("prompt number 69", { exact: true })).toHaveCount(0);
  await expect(page.getByText("prompt number 70", { exact: true })).toHaveCount(1);
  await page.getByTestId("acp-load-earlier").evaluate((element) => (element as HTMLButtonElement).click());
  await expect(prompts).toHaveCount(320);
  await expect(page.getByText("prompt number 0", { exact: true })).toHaveCount(1);
});
