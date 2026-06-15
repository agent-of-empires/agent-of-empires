// User story (#2144): opening a long structured-view transcript renders
// the most recent slice first, so the user lands at the latest message
// instead of waiting for the whole backlog to paint. Older turns are
// revealed by the "Load earlier messages" button, a chunk at a time.
//
// Seeds 100 turns (a UserPromptSent + an agent reply each = 200 activity
// rows, past the 150-row default window) and asserts the oldest turn is
// not painted until the user loads earlier history.

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
  for (let i = 0; i < 3; i += 1) {
    if ((await page.getByText("prompt number 0").count()) > 0) break;
    await loadEarlier.click();
  }
  await expect(page.getByText("prompt number 0")).toBeVisible({ timeout: 10_000 });
});
