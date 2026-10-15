import { test, expect } from "./helpers/mockedTest";
import { mockStructuredSessionApis, openStructuredViewFor } from "./helpers/structuredSessionMocks";
import { devices, type Page } from "@playwright/test";

// Tapping the structured-view transcript must NOT focus the composer / open the
// soft keyboard: the keyboard should open only when the user taps the input
// itself. (Tap-to-focus, #2243, was reverted for SV because it popped the
// keyboard whenever you tapped anywhere in the output.) On a coarse pointer the
// composer is not auto-focused on mount (#1178), so it starts unfocused.

test.use({ ...devices["iPhone 13"] });

const SESSION_ID = "sess-acp-tap";
const TITLE = "acp-tap";

async function setup(page: Page) {
  await mockStructuredSessionApis(page, { id: SESSION_ID, title: TITLE, projectPath: "/tmp/acp-tap" });
}

const openStructuredSession = (page: Page) => openStructuredViewFor(page, TITLE);

test.describe("Structured-view transcript tap does not open the keyboard", () => {
  test("tapping the transcript does not focus the composer; tapping the input does", async ({ page }) => {
    await setup(page);
    await openStructuredSession(page);

    const composer = page.getByPlaceholder(/Send a message/);
    await expect(composer).toBeVisible();
    // Establish a known-unfocused state so the transcript tap is the only thing
    // that could move focus.
    await composer.blur();
    await expect(composer).not.toBeFocused();

    // Tapping an empty area of the transcript must NOT move focus into the
    // composer (so the soft keyboard stays closed).
    await page.getByTestId("acp-viewport").click({ position: { x: 8, y: 8 } });
    await expect(composer).not.toBeFocused();

    // Tapping the input itself is the only thing that focuses it (opens the
    // keyboard).
    await composer.click();
    await expect(composer).toBeFocused();
  });
});
