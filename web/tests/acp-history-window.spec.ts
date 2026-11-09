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

test("opening a cached session keeps its readable window through delayed catch-up", async ({ page }) => {
  const mock = await mockAcpSession(page, { title: "stable-warm-window", initialEvents: longTranscript() });
  await openStructuredSession(page, mock);
  await waitForComposerConnected(page);
  const key = `aoe:acp-state:v1:${mock.sessionId}`;
  await expect
    .poll(() =>
      page.evaluate((key) => {
        const entry = JSON.parse(localStorage.getItem(key) ?? "null");
        return entry?.state?.activity?.filter((row: { kind: string }) => row.kind === "user_prompt").length ?? 0;
      }, key),
    )
    .toBe(TURNS);
  let release!: () => void;
  let requested!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  const started = new Promise<void>((resolve) => {
    requested = resolve;
  });
  await page.route(/\/acp\/replay(\?|$)/, async (route) => {
    requested();
    await held;
    await route.fallback();
  });
  const viewport = page.getByTestId("acp-viewport");
  try {
    await page.reload();
    await started;
    await expect(viewport.locator('[data-message-id="user-seq-151"]')).toHaveCount(1);
    const before = await viewport
      .locator("[data-message-id]")
      .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-message-id")));
    await viewport.evaluate((element) => {
      const expected = Array.from(element.querySelectorAll("[data-message-id]")).map((node) =>
        node.getAttribute("data-message-id"),
      );
      const monitor = { missing: [] as string[], observer: null as MutationObserver | null };
      monitor.observer = new MutationObserver(() => {
        for (const id of expected) {
          if (!element.querySelector(`[data-message-id="${id}"]`)) monitor.missing.push(id!);
        }
      });
      monitor.observer.observe(element, { childList: true, subtree: true });
      (window as typeof window & { __warmWindowMonitor?: typeof monitor }).__warmWindowMonitor = monitor;
    });
    release();
    await waitForComposerConnected(page);
    const missing = await page.evaluate(() => {
      const monitor = (
        window as typeof window & { __warmWindowMonitor?: { missing: string[]; observer: MutationObserver } }
      ).__warmWindowMonitor!;
      monitor.observer.disconnect();
      return monitor.missing;
    });
    expect(missing).toEqual([]);
    const after = await viewport
      .locator("[data-message-id]")
      .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-message-id")));
    expect(after).toEqual(before);
    await expect(page.getByText("reply number 99", { exact: true })).toBeVisible();
    expect(mock.promptBodies).toEqual([]);
  } finally {
    release();
  }
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
