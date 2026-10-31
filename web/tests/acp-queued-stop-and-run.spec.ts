// A solo command queued behind a steerable turn can't steer in, so its row stops the turn instead.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

test("Stop and run on a queued /compact cancels a steerable turn; the message row still steers", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-stop-and-run",
    sessionFields: { clear_aliases: ["/clear"], compact_aliases: ["/compact"] },
    initialEvents: [
      { PromptCapabilities: { image: true, audio: false, embedded_context: true, steering: true } },
      { UserPromptSent: { text: "Refactor the parser" } },
    ],
  });
  const at = new Date().toISOString();
  await page.route("**/api/sessions/*/queue", (r) =>
    r.fulfill({
      json: [
        { id: "q1", seq: 1, text: "Also update the changelog", created_at: at },
        { id: "q2", seq: 2, text: "/compact keep the parser notes", created_at: at },
      ],
    }),
  );
  await openStructuredSession(page, mock);
  const actions = page.getByTestId("queued-send-now");
  await expect(actions).toHaveCount(2);
  await expect(actions.nth(0)).toHaveAccessibleName("Send this queued message now");
  await expect(actions.nth(1)).toHaveText("Stop and run");

  const cancel = page.waitForRequest((r) => r.method() === "POST" && /\/acp\/cancel$/.test(new URL(r.url()).pathname));
  await actions.nth(1).click();
  await cancel;
  // Stopping doesn't send the command itself: the Daemon drains the queue in order once the turn ends.
  expect(mock.promptBodies).toEqual([]);
});
