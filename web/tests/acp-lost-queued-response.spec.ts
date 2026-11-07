// #5401: a prompt the daemon queued, whose POST response never reaches the tab, must not leave
// the composer busy or a ghost user bubble once the queue lists it.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

test("a queued prompt whose POST response is lost clears the busy composer and the ghost bubble", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-lost-queued-response",
    initialEvents: [
      { AcpSessionAssigned: { acp_session_id: "acp-story" } },
      { PromptCapabilities: { image: true, audio: false, embedded_context: true, steering: true } },
    ],
  });
  let sentId = "";
  let promptSeen!: () => void;
  const seen = new Promise<void>((resolve) => (promptSeen = resolve));
  // Never fulfilled: the daemon answered, but the response did not land in this tab.
  await page.route("**/api/sessions/*/acp/prompt", (r) => {
    sentId = (JSON.parse(r.request().postData() ?? "{}") as { prompt_id?: string }).prompt_id ?? "";
    promptSeen();
  });
  let releaseQueue!: () => void;
  const gate = new Promise<void>((resolve) => (releaseQueue = resolve));
  await page.route("**/api/sessions/*/queue", async (r) => {
    if (r.request().method() !== "GET") return r.fallback();
    await seen;
    await gate;
    await r.fulfill({
      json: [{ id: sentId, seq: 1, text: "queued during compact", created_at: new Date().toISOString() }],
    });
  });
  await openStructuredSession(page, mock);

  const composer = page.getByRole("textbox", { name: /Send a message/i });
  await composer.fill("queued during compact");
  await composer.press("Enter");
  await seen;
  await expect(page.getByRole("button", { name: "Stop" })).toBeVisible();
  expect(mock.promptBodies).toEqual([]);

  releaseQueue();
  await expect(page.getByTestId("queued-strip-header")).toBeVisible();
  await expect(page.getByRole("button", { name: "Stop" })).toHaveCount(0);
  await expect(page.getByText("queued during compact")).toHaveCount(1);
});
