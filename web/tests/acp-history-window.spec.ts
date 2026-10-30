// #2144: a long transcript renders its recent slice first; 100 turns (200 rows) exceed the 150-row window.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
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

function longTranscript(): unknown[] {
  const events: unknown[] = [];
  for (let i = 0; i < TURNS; i += 1) {
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
