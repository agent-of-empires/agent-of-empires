// User story (#2144): opening a long structured-view transcript renders
// the most recent slice first, so the user lands at the latest message
// instead of waiting for the whole backlog to paint. Older turns are
// revealed by the "Load earlier messages" button, a chunk at a time.
//
// Seeds 100 turns (a UserPromptSent + an agent reply each = 200 activity
// rows, past the 150-row default window) and asserts the oldest turn is
// not painted until the user loads earlier history.

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

// Click "Load earlier" until the oldest turn (`prompt number 0`) surfaces.
// The button is conditionally mounted and disables itself while a server
// `before` fetch is in flight (StructuredView `canLoadEarlierHistory` /
// `loadingEarlierHistory`), so it unmounts and remounts mid-interaction. A
// fixed-interval click loop races that re-render and the click lands on a
// detaching node ("element was detached from the DOM"), which flaked #2236's
// network-paging test. Re-resolve the button on every poll and bound each
// click, so a click waits out an in-flight load instead of racing it.
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

  // Recent turn is painted on open.
  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });

  // The oldest turn is windowed out (200 rows, 150-row default window).
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  // The control to widen the window is offered.
  const loadEarlier = page.getByTestId("acp-load-earlier");
  await expect(loadEarlier).toBeVisible();

  // Growing the window enough times reveals the oldest turn.
  await revealOldestTurn(page);
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

// User story (#2236): scrolling to the top auto-loads earlier messages,
// no button click needed.
test("scrolling to the top auto-loads earlier messages", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-history-autoload",
    initialEvents: longTranscript(),
  });
  await openStructuredSession(page, mock);

  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  // Drive the viewport to the top; the scroll handler should reveal more
  // history without a button click.
  await page.getByTestId("acp-viewport").evaluate((el) => {
    el.scrollTop = 0;
  });
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

// User story (#2236, feature C): a transcript larger than one replay page
// loads recent-first and fetches still-older events from the server when
// the already-loaded rows are exhausted (network paging, not just the
// in-reducer window).
test("loads older events from the server when the loaded window is exhausted", async ({ page }) => {
  // 350 turns = 1050 events, past the client's 1000-event page, so the
  // tail leaves older history on the server (has_more) reachable only via
  // a `before` fetch.
  const events: unknown[] = [];
  for (let i = 0; i < 350; i += 1) {
    events.push(userPrompt(`prompt number ${i}`));
    events.push(agentMessageChunk(`reply number ${i}`));
    events.push(stopped());
  }
  const mock = await mockAcpSession(page, { title: "story-history-network", initialEvents: events });
  await openStructuredSession(page, mock);

  await expect(page.getByText("reply number 349")).toBeVisible({ timeout: 10_000 });
  // Turn 0 is not in the recent page at all; it must be fetched.
  await expect(page.getByText("prompt number 0")).toHaveCount(0);

  // Reveal loaded rows, then trip the server `before` fetch, until the very
  // first turn surfaces.
  await revealOldestTurn(page);
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});

// User story (#2236, symptom A): a new turn must not fold earlier rows
// already in the window back behind "Load earlier".
test("a new turn does not re-fold earlier messages", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-history-nofold",
    initialEvents: longTranscript(),
  });
  await openStructuredSession(page, mock);

  await expect(page.getByText(`reply number ${TURNS - 1}`)).toBeVisible({ timeout: 10_000 });
  // 200 rows, 150-row window snapped to a user boundary: turn 25 is the
  // oldest turn in the window and is rendered (off-screen but in the DOM).
  await expect(page.getByText("prompt number 25")).toHaveCount(1);

  // The agent streams several new turns.
  for (let i = 0; i < 20; i += 1) {
    mock.pushEvents([userPrompt(`fresh prompt ${i}`), agentMessageChunk(`fresh reply ${i}`), stopped()]);
  }
  await expect(page.getByText("fresh reply 19")).toBeVisible({ timeout: 10_000 });

  // Pre-fix the window would have slid forward ~40 rows and dropped turn
  // 25; with the anchor-on-append fix it stays rendered.
  await expect(page.getByText("prompt number 25")).toHaveCount(1);
});

// User story (#2144): when /clear is active and the windowed-out rows are
// all before the clear divider, "Load earlier" would be a no-op (those
// turns are reached via the cleared-turns banner, not this control), so
// the button must not appear.
test("Load earlier stays hidden when only pre-clear rows are windowed out", async ({ page }) => {
  const events: unknown[] = [];
  // 100 pre-clear turns (200 rows) so the window cut lands well before
  // the clear divider, then a /clear and a couple of short post-clear turns.
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

  // Post-clear content renders; pre-clear turns are folded behind the banner.
  await expect(page.getByText("new reply 1")).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("old prompt 0")).toHaveCount(0);

  // The windowed-out rows are all pre-clear, so the control is suppressed.
  await expect(page.getByTestId("acp-load-earlier")).toHaveCount(0);
});
