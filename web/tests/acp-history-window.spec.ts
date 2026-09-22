// #2144: a long transcript renders its recent slice first; 100 turns (200 rows) exceed the 150-row window.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, agentMessageChunk, stopped } from "./helpers/acpMock";

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

test("long transcript renders recent first and reveals older on Load earlier", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-history-window",
    initialEvents: longTranscript(),
  });
  await openStructuredSession(page, mock);

  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });

  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  const loadEarlier = page.getByTestId("acp-load-earlier");
  await expect(loadEarlier).toBeVisible();

  await revealOldestTurn(page);
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

// #2236: scrolling to the top loads earlier messages without a click.
test("scrolling to the top auto-loads earlier messages", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-history-autoload",
    initialEvents: longTranscript(),
  });
  await openStructuredSession(page, mock);

  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  await page.getByTestId("acp-viewport").evaluate((el) => {
    el.scrollTop = 0;
  });
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
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  await revealOldestTurn(page);
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

// #2236: new turns must not push rows already in the window back behind "Load earlier".
test("a new turn does not re-fold earlier messages", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-history-nofold",
    initialEvents: longTranscript(),
  });
  await openStructuredSession(page, mock);

  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });
  // Turn 25 is the oldest turn in the window.
  await expect(page.getByText("prompt number 25")).toHaveCount(1);

  for (let i = 0; i < 20; i += 1) {
    mock.pushEvents([userPrompt(`fresh prompt ${i}`), agentMessageChunk(`fresh reply ${i}`), stopped()]);
  }
  await expect(page.getByText("fresh reply 19")).toBeVisible({ timeout: 10_000 });

  await expect(page.getByText("prompt number 25")).toHaveCount(1);
});

// #2144: rows hidden only before a /clear are reached via the cleared-turns banner, so no "Load earlier".
test("Load earlier stays hidden when only pre-clear rows are windowed out", async ({ page }) => {
  const events: unknown[] = [];
  for (let i = 0; i < 100; i += 1) {
    events.push(userPrompt(`old prompt ${i}`));
    events.push(agentMessageChunk(`old reply ${i}`));
    events.push(stopped());
  }
  events.push("SessionCleared");
  for (let i = 0; i < 2; i += 1) {
    events.push(userPrompt(`new prompt ${i}`));
    events.push(agentMessageChunk(`new reply ${i}`));
    events.push(stopped());
  }

  const mock = await mockAcpSession(page, { title: "story-history-window-clear", initialEvents: events });
  await openStructuredSession(page, mock);

  await expect(page.getByText("new reply 1")).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("old prompt 0")).toHaveCount(0);

  await expect(page.getByTestId("acp-load-earlier")).toHaveCount(0);
});
