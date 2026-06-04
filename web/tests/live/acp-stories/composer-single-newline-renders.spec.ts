// User story (#1472): a single newline in the composer is preserved in
// the sent user message.
//
// The composer is a plain <textarea>, so a lone shift+enter shows as a
// visible line break while typing. Before the fix the sent bubble ran
// through remark-gfm only and collapsed single newlines to whitespace.
// This drives the real structured view render path (Markdown.tsx -> UserText
// with breaks enabled): type three lines separated by single newlines,
// send, and assert the rendered user bubble keeps them on separate rows
// (two <br> nodes), not one wrapped paragraph.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

base("single newlines in a user message render as line breaks", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-single-newline" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-single-newline");
    if (!seeded) throw new Error("seeded session 'story-single-newline' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    // fill() sets the textarea value verbatim, including the newlines a
    // shift+enter would have inserted; Enter then sends the whole thing.
    await composer.fill("line a\nline b\nline c");
    await composer.press("Enter");

    // The sent user bubble (rounded-br-sm, right-aligned) must preserve
    // the three lines as separate rows: two hard breaks between them.
    const userBubble = page
      .locator("div.rounded-br-sm")
      .filter({ hasText: "line a" });
    await expect(userBubble).toBeVisible({ timeout: 10_000 });
    await expect(userBubble.locator("br")).toHaveCount(2);
    await expect(userBubble).toContainText("line b");
    await expect(userBubble).toContainText("line c");
  } finally {
    await serve.stop();
  }
});
