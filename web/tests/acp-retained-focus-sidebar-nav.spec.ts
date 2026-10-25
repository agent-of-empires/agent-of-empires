import { test, expect } from "./helpers/mockedTest";
import { mockStructuredSessionApis } from "./helpers/structuredSessionMocks";
import { mockSessionsList } from "./helpers/sessions";
import { clickSidebarSession } from "./helpers/sidebar";
import type { Page } from "@playwright/test";

// Desktop fine pointer with retained structured views: selecting a retained
// session in the sidebar must hand typing focus to that session's composer
// once the selection commits (#4777), not to the view it replaces.

test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

const SESSIONS = [
  { id: "sess-focus-a", title: "focus-a" },
  { id: "sess-focus-b", title: "focus-b" },
];

const selectedComposer = (page: Page) => page.locator('[aria-hidden="false"]').getByPlaceholder(/Send a message/);
const retainedComposer = (page: Page) => page.locator('[aria-hidden="true"]').getByPlaceholder(/Send a message/);

// Controlled time flushes the 250/700 ms mount-focus reclaim timers (Composer useInitialFocus)
// so they cannot mask a missing activation effect on the return trip.
const settleInitialFocus = (page: Page) => page.clock.runFor(1_000);

async function selectViaSidebar(page: Page, title: string, id: string) {
  await clickSidebarSession(page, title);
  await expect(page).toHaveURL(new RegExp(`/session/${id}$`));
  await expect(selectedComposer(page)).toBeVisible();
}

test("sidebar A to B to A focuses retained A's composer and typing reaches only A", async ({ page }) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ persistentStructuredViews: true }));
  });
  await page.clock.install();
  await mockStructuredSessionApis(page, { id: SESSIONS[0]!.id, title: SESSIONS[0]!.title });
  await mockSessionsList(page, () =>
    SESSIONS.map((s) => ({
      ...s,
      project_path: `/tmp/${s.title}`,
      group_path: "/tmp",
      status: "Running",
      view: "structured",
      acp_worker_state: "running",
      claude_fullscreen: false,
    })),
  );
  await page.goto("/");

  await selectViaSidebar(page, "focus-a", "sess-focus-a");
  // Retention keeps this DOM node alive across the A to B to A round trip.
  await selectedComposer(page).evaluate((el) => el.setAttribute("data-test-owner", "A"));
  await settleInitialFocus(page);

  await selectViaSidebar(page, "focus-b", "sess-focus-b");
  await expect(retainedComposer(page)).toHaveAttribute("data-test-owner", "A");
  await settleInitialFocus(page);

  await selectViaSidebar(page, "focus-a", "sess-focus-a");
  const composerA = selectedComposer(page);
  await expect(composerA).toHaveAttribute("data-test-owner", "A");
  await expect(composerA).toBeFocused();

  await page.keyboard.type("only-a");
  await expect(composerA).toHaveValue("only-a");
  await expect(retainedComposer(page)).toHaveValue("");
});
