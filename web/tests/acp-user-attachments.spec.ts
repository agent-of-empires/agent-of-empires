import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const SHOTS = process.env.USER_ATTACHMENT_SHOTS;

// A 240x120 grayscale PNG, so the browser reports a real natural size.
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAPAAAAB4CAAAAADp3SD7AAABYUlEQVR4nOXOMQEAAAyDMKRX+lzAsSgIe4Y6YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioAzbqgI06YKMO2KgDNuqAjTpgow7YqAM26oCNOmCjDtioA7YDpo9DSWZzh7UAAAAASUVORK5CYII=",
  "base64",
);

test("a prompt's image and document share one bubble with the text and open on click", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-user-attachments",
    initialEvents: [
      {
        UserPromptSent: {
          text: "what is wrong here?",
          attachments: [
            { id: "att-img", kind: "image", mime_type: "image/png", name: "screenshot.png", size: PNG.length },
            { id: "att-doc", kind: "resource", mime_type: "text/plain", name: "notes.txt", size: 5 },
          ],
        },
      },
      stopped(),
    ],
  });
  // Registered after the mock: Playwright tries the latest route first, and the mock's catch-all would win.
  await page.route("**/api/sessions/sess-1/acp/attachments/att-img", (route) =>
    route.fulfill({ contentType: "image/png", body: PNG }),
  );
  await page.route("**/api/sessions/sess-1/acp/attachments/att-doc", (route) =>
    route.fulfill({ contentType: "text/plain", body: "hello" }),
  );
  await openStructuredSession(page, mock);

  const bubble = page.getByTestId("user-message-bubble");
  await expect(bubble).toHaveCount(1);
  const image = bubble.locator("img.acp-artifact-image");
  await expect(image).toBeVisible();
  await expect(bubble.getByRole("button", { name: /notes\.txt/ })).toBeVisible();
  await expect(bubble.getByText("what is wrong here?")).toBeVisible();

  // Natural aspect ratio survives inside the bubble.
  await expect.poll(() => image.evaluate((el: HTMLImageElement) => el.naturalWidth)).toBe(240);
  const ratio = await image.evaluate((el: HTMLImageElement) => el.clientWidth / el.clientHeight);
  expect(ratio).toBeCloseTo(2, 1);
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/user-attachments.png` });

  await image.click();
  const lightbox = page.getByTestId("image-lightbox");
  await expect(lightbox).toBeVisible();
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/user-attachments-lightbox.png` });
  await page.keyboard.press("Escape");
  await expect(lightbox).toHaveCount(0);

  const popup = page.waitForEvent("popup");
  await bubble.getByRole("button", { name: /notes\.txt/ }).click();
  const tab = await popup;
  // The opener closes the tab when the fetch fails, so require a navigated, still-open blob tab.
  await tab.waitForURL(/^blob:/);
  expect(tab.isClosed()).toBe(false);
});

test("the lightbox holds back the terminal-focus shortcut while a paired terminal is mounted", async ({ page }) => {
  // The ACP mock is registered last so its session list wins; the terminal mock only supplies the paired shell's routes.
  await mockTerminalApis(page);
  const mock = await mockAcpSession(page, {
    title: "story-lightbox-shortcut",
    initialEvents: [
      {
        UserPromptSent: {
          text: "look",
          attachments: [{ id: "att-img", kind: "image", mime_type: "image/png", name: "shot.png", size: PNG.length }],
        },
      },
      stopped(),
    ],
  });
  await page.route("**/api/sessions/sess-1/acp/attachments/att-img", (route) =>
    route.fulfill({ contentType: "image/png", body: PNG }),
  );
  await openStructuredSession(page, mock);

  await page.locator('[data-testid^="pane-tab-terminal:"]').first().click();
  const pairedInput = page.locator('[data-term="paired"]:visible textarea').first();
  await expect(pairedInput).toBeAttached();

  await page.locator("img.acp-artifact-image").click();
  const lightbox = page.getByTestId("image-lightbox");
  await expect(lightbox).toBeVisible();
  const close = lightbox.getByRole("button", { name: "Close image" });
  await expect(close).toBeFocused();

  await page.keyboard.press("ControlOrMeta+`");
  await expect(lightbox).toBeVisible();
  await expect(close).toBeFocused();
  await expect(pairedInput).not.toBeFocused();
});
