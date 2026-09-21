// Follow-up queue: the first turn is held open so the composer offers Queue instead of Send.

import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle, type ServeOptions } from "../../helpers/liveTest";
import {
  HOLD,
  chunk,
  endTurn,
  openStructuredView,
  replayFrames,
  releaseTurn,
  script,
  startAcpSession,
  waitForReplayContains,
  waitForStructuredView,
} from "../../helpers/acp";

/** Start a held first turn and wait until the Queue button replaces Send. */
async function startHeldTurn(
  page: Page,
  spawnServe: (opts?: ServeOptions) => Promise<ServeHandle>,
  title: string,
  secondTurnText?: string,
) {
  const turns = [endTurn(chunk("First turn."), HOLD)];
  if (secondTurnText) turns.push(endTurn(chunk(secondTurnText)));
  const { serve, sessionId } = await startAcpSession(spawnServe, { title, fakeAcpScript: script(...turns) });
  const composer = await openStructuredView(page, serve, sessionId, "kick off");
  await expect(page.getByText("First turn.")).toBeVisible({ timeout: 10_000 });
  // The Queue button renders only while the turn is active.
  const queueBtn = page.getByRole("button", { name: /Queue follow-up message/i });
  await expect(queueBtn).toBeVisible({ timeout: 5_000 });
  const queue = async (text: string) => {
    await composer.fill(text);
    await queueBtn.click();
  };
  return { serve, sessionId, queue };
}

async function leaveForSettings(page: Page, serve: ServeHandle) {
  await page.goto(`${serve.baseUrl}/settings`);
  await expect(page).toHaveURL(/\/settings/, { timeout: 10_000 });
  await expect(page.locator("select").first()).toBeVisible({ timeout: 10_000 });
}

test("queued follow-up fires when first turn ends", async ({ page, spawnServe }) => {
  const { serve, queue } = await startHeldTurn(page, spawnServe, "story-queue", "Second turn response.");
  await queue("second please");
  await expect(page.getByText(/Queued \(1\)/i)).toBeVisible();
  releaseTurn(serve);
  await expect(page.getByText("Second turn response.")).toBeVisible({ timeout: 15_000 });
});

test("delete a queued follow-up before it fires", async ({ page, spawnServe }) => {
  const { queue } = await startHeldTurn(page, spawnServe, "story-queue-del");
  await queue("doomed queued text");
  const queuedRow = page.getByRole("button", { name: /^doomed queued text$/ });
  await expect(queuedRow).toBeVisible({ timeout: 5_000 });

  await page.getByTitle("Drop this queued message").click();
  await expect(queuedRow).toHaveCount(0, { timeout: 5_000 });
});

test("edit a queued follow-up before it fires", async ({ page, spawnServe }) => {
  const { queue } = await startHeldTurn(page, spawnServe, "story-queue-edit");
  await queue("original queued text");
  const queuedRow = page.getByRole("button", { name: /^original queued text$/ });
  await expect(queuedRow).toBeVisible({ timeout: 5_000 });

  await queuedRow.click();
  // The row click blurs the composer and the editor autofocuses, so :focus is the editor.
  const editor = page.locator("textarea:focus");
  await expect(editor).toBeVisible({ timeout: 5_000 });
  await editor.fill("edited queued text");
  await editor.press("Enter");

  await expect(page.getByRole("button", { name: /^edited queued text$/ })).toBeVisible({ timeout: 5_000 });
  await expect(page.getByRole("button", { name: /^original queued text$/ })).toHaveCount(0);
});

test("a queued follow-up drains while its chat is closed", async ({ page, spawnServe }) => {
  // #3331: never return to the session; `page.goto` reloads, exercising the localStorage rearm path.
  const queuedText = "drained-while-away";
  const turnTwoText = "Second turn while away.";
  const { serve, sessionId, queue } = await startHeldTurn(page, spawnServe, "queue-unmounted-a", turnTwoText);
  await queue(queuedText);
  // Also guarantees the queue reached localStorage before the reload.
  await expect(page.getByText(/Queued \(1\)/i)).toBeVisible({ timeout: 5_000 });

  await leaveForSettings(page, serve);
  releaseTurn(serve);
  await waitForReplayContains(serve.baseUrl, sessionId, [queuedText, turnTwoText], { mode: "all", timeoutMs: 25_000 });
  await expect(page).toHaveURL(/\/settings/);

  // The fake falls back to a generic turn once scripted turns run out, so count user sends, not agent chunks.
  const frames = (await replayFrames(serve.baseUrl, sessionId)) as { event?: { UserPromptSent?: { text?: string } } }[];
  expect(frames.filter((f) => f.event?.UserPromptSent?.text === queuedText)).toHaveLength(1);
});

test("queued follow-up fires after navigation away and back", async ({ page, spawnServe }) => {
  const { serve, sessionId, queue } = await startHeldTurn(page, spawnServe, "queue-nav-a", "Second turn after nav.");
  await queue("from-after-nav");

  // A document navigation aborts an in-flight enqueue POST, so wait until the daemon owns the row.
  const queueUrl = `${serve.baseUrl}/api/sessions/${encodeURIComponent(sessionId)}/queue`;
  await expect
    .poll(async () => ((await fetch(queueUrl).then((r) => r.json())) as Array<{ text: string }>).map((r) => r.text), {
      timeout: 10_000,
    })
    .toEqual(["from-after-nav"]);

  await leaveForSettings(page, serve);
  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
  await waitForStructuredView(page);

  releaseTurn(serve);
  // Count 1 catches a double fire after remount.
  await expect(page.getByText("Second turn after nav.", { exact: true })).toHaveCount(1, { timeout: 20_000 });
});

test("sidebar row shows the queued-prompt count badge", async ({ page, spawnServe }) => {
  const { serve, sessionId, queue } = await startHeldTurn(page, spawnServe, "sidebar-queue-a");
  const promptEndpoint = `${serve.baseUrl}/api/sessions/${encodeURIComponent(sessionId)}/acp/prompt`;
  for (const text of ["follow-up one", "follow-up two"]) {
    const responsePromise = page.waitForResponse(
      (response) => response.url() === promptEndpoint && response.request().method() === "POST",
    );
    await queue(text);
    const response = await responsePromise;
    expect(response.status()).toBe(202);
    expect(await response.json()).toMatchObject({ disposition: "queued" });
  }
  // Only confirmed rows are persisted; check the local projection before the hard navigation.
  await expect(page.getByText("Queued (2)", { exact: true })).toBeVisible();

  await page.goto(serve.baseUrl);
  await expect(page.getByTitle("2 queued prompts")).toBeVisible({ timeout: 15_000 });
});
