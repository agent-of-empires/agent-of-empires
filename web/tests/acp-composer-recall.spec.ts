import { test, expect, type Page } from "./helpers/mockedTest";
import { mockStructuredSessionApis, openStructuredViewFor } from "./helpers/structuredSessionMocks";

// Queue-recall behavior for the structured-view composer (#2147), driven
// through the real component in mocked mode so the ArrowUp/ArrowDown
// handlers, the "Editing queued message" banner, Esc-restore, and the
// edit-in-place submit path all execute in the browser.
//
// Sending the first prompt flips the session turn-active (optimistic
// user_prompt dispatch), so subsequent submissions park in the queue
// rather than sending. From there the arrows browse the queue.

const SESSION_ID = "sess-acp-recall";
const TITLE = "acp-recall";

async function setup(page: Page) {
  await mockStructuredSessionApis(page, { id: SESSION_ID, title: TITLE, projectPath: "/tmp/acp-recall" });
  // The daemon owns the send / queue decision (Tier 3): the first prompt opens
  // the turn and every follow-up parks behind it. Registered after the shared
  // acp/** route so it wins Playwright's reverse-registration-order matching.
  let promptPosts = 0;
  await page.route(/\/acp\/prompt(\?|$)/, (r) => {
    promptPosts += 1;
    if (promptPosts === 1) return r.fulfill({ json: { disposition: "sent" } });
    const id = `srv-q${promptPosts}`;
    return r.fulfill({ json: { disposition: "queued", reason: "turn_active", queued_id: id } });
  });
}

const openStructuredSession = (page: Page) => openStructuredViewFor(page, TITLE);

test.describe("Structured-view composer queue recall (#2147)", () => {
  test("ArrowUp recalls queued prompts, banner + Esc + edit-in-place", async ({ page }) => {
    await setup(page);
    await openStructuredSession(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });

    // Send the first prompt to put the turn active, then queue two follow-ups.
    await composer.fill("kick off");
    await composer.press("Enter");
    const queueButton = page.getByRole("button", { name: /Queue follow-up message/i });
    await expect(queueButton).toBeVisible({ timeout: 10000 });

    await composer.fill("first queued");
    await queueButton.click();
    await composer.fill("second queued");
    await queueButton.click();
    await expect(page.getByRole("button", { name: /^second queued$/ })).toBeVisible({ timeout: 5000 });

    // Empty composer: ArrowUp enters recall on the newest, banner shows.
    await expect(composer).toHaveValue("");
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await expect(page.getByText(/Editing queued message 1 of 2/)).toBeVisible();

    // ArrowDown past the newest restores the stashed (empty) draft and exits.
    await composer.press("ArrowDown");
    await expect(composer).toHaveValue("");
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);

    // Re-enter and walk to the oldest.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("first queued");
    await expect(page.getByText(/Editing queued message 2 of 2/)).toBeVisible();

    // ArrowUp at the oldest is a no-op (no wrap): stays on the oldest.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("first queued");
    await expect(page.getByText(/Editing queued message 2 of 2/)).toBeVisible();

    await composer.press("ArrowDown");
    await expect(composer).toHaveValue("second queued");

    // Esc restores the stashed (empty) draft and clears the banner.
    await composer.press("Escape");
    await expect(composer).toHaveValue("");
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);

    // Re-enter, edit, submit: the queued entry updates in place, no dup.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await composer.fill("second queued edited");
    await composer.press("Enter");
    await expect(page.getByRole("button", { name: /^second queued edited$/ })).toBeVisible({ timeout: 5000 });
    await expect(page.getByRole("button", { name: /^second queued$/ })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^first queued$/ })).toBeVisible();
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);
  });
});
