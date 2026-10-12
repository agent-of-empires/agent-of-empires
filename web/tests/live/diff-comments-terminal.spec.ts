import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { seedSessionViaAoeAdd } from "../helpers/aoeServe";

const TITLE = "diff-comments-live";
const COMMENT = "LIVE_DIFF_COMMENT_MARKER: fix the greeting";
const INTRO = "LIVE_DIFF_INTRO_MARKER: review this";

async function openSession(page: Page, serve: ServeHandle) {
  await page.goto(`${serve.baseUrl}/`);
  const row = page.getByRole("link").filter({ hasText: TITLE }).first();
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click();
  await expect(page.getByText("1 file", { exact: true }).first()).toBeVisible({ timeout: 15_000 });
  await page
    .getByRole("button", { name: /notes\.md/ })
    .first()
    .click();
  await page.getByRole("button", { name: "Raw", exact: true }).first().click();
}

async function addDiffComment(page: Page) {
  const changedLine = page.locator("[data-line-number-content]").filter({ hasText: /^2$/ }).last();
  await expect(changedLine).toBeVisible({ timeout: 10_000 });
  await changedLine.click();
  const comment = page.getByPlaceholder(/Leave a comment \(markdown supported\)/);
  await expect(comment).toBeVisible();
  await comment.fill(COMMENT);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText(/^1 comment$/).first()).toBeVisible();
}

function commentSession() {
  return seedSessionViaAoeAdd({
    title: TITLE,
    committed: { "notes.md": "alpha\nbeta\ngamma\n" },
    files: { "notes.md": "alpha\nbeta changed\ngamma\n" },
    // The real terminal /send endpoint pastes the prompt into this isolated agent pane.
    agentScript: `#!/bin/bash\nprintf 'DIFF_COMMENT_AGENT_READY\\n'\nwhile IFS= read -r line; do printf 'AGENT_RECEIVED: %s\\n' "$line"; done\n`,
  });
}

test("terminal diff comments reach the agent pane through the real send endpoint", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: commentSession() });

  await openSession(page, serve);
  const agentPane = page.locator('[data-term="agent"] [data-live-content]').first();
  await expect(agentPane).toContainText("DIFF_COMMENT_AGENT_READY", { timeout: 30_000 });
  await addDiffComment(page);
  await page
    .getByRole("button", { name: /^Send$/ })
    .first()
    .click();
  await expect(page.getByText("Send diff comments")).toBeVisible();
  await page.getByPlaceholder(/Anything you want to say/).fill(INTRO);

  const sendRequest = (request: { method(): string; url(): string }) =>
    request.method() === "POST" && /\/api\/sessions\/[^/]+\/send$/.test(request.url());
  const requestPromise = page.waitForRequest(sendRequest, { timeout: 15_000 });
  const responsePromise = page.waitForResponse((response) => sendRequest(response.request()), { timeout: 15_000 });
  await page
    .getByRole("button", { name: /^Send$/ })
    .last()
    .click();
  const [request, response] = await Promise.all([requestPromise, responsePromise]);
  expect(response.ok()).toBe(true);

  const payload: unknown = request.postDataJSON();
  let message: string | undefined;
  if (typeof payload === "object" && payload !== null && "message" in payload && typeof payload.message === "string") {
    message = payload.message;
  }
  expect(message).toContain(INTRO);
  expect(message).toContain("## Diff comments");
  expect(message).toContain("notes.md");
  expect(message).toContain("line 2 (new)");
  expect(message).toContain(COMMENT);
  expect(message).toContain("Please address these comments.");
  expect(message).not.toContain("aoe:diff-comments");
  await expect(agentPane).toContainText(`AGENT_RECEIVED: ${COMMENT}`, { timeout: 30_000 });
  await expect(agentPane).toContainText("AGENT_RECEIVED: Please address these comments.");
});

test("read-only terminal diff comments cannot send to the agent pane", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ readOnly: true, seedFn: commentSession() });
  const sends: string[] = [];
  page.on("request", (request) => {
    if (request.method() === "POST" && /\/api\/sessions\/[^/]+\/send$/.test(request.url())) sends.push(request.url());
  });

  await openSession(page, serve);
  await addDiffComment(page);
  const send = page.getByRole("button", { name: /^Send$/ }).first();
  await expect(send).toHaveAttribute("aria-disabled", "true");
  await send.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByText("Send diff comments")).toHaveCount(0);
  expect(sends).toEqual([]);
});
