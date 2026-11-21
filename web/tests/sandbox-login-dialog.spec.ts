// The sandbox login dialog in a real browser: initial focus, focus after a rejected code, the Tab order,
// Escape, and that a typed code survives a validation error. SandboxLoginDialog.test.tsx pins the request
// payloads and error mapping against the server contract.

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis, mockStaticApis } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";

const URL_ONE = "https://login.example.test/authorize?code=true&state=SAMPLE-ONE";

async function installLoginMocks(page: Page) {
  const cancels: string[] = [];
  const codes: string[] = [];
  await mockStaticApis(page);
  await mockSettingsApis(page);
  await page.route(
    (url) => url.pathname === "/api/sandbox/login",
    (route) => {
      if (route.request().method() === "POST") {
        return route.fulfill({ json: { login_id: "login-1", url: URL_ONE } });
      }
      return route.fulfill({ json: { status: { state: "needed" }, running: null, held_sessions: 0 } });
    },
  );
  await page.route("**/api/sandbox/login/login-1/code", (route) => {
    const { code } = route.request().postDataJSON() as { code: string };
    codes.push(code);
    return code.includes(" ")
      ? route.fulfill({ status: 400, json: { error: "invalid_code", message: "The code has a space in it." } })
      : route.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/sandbox/login/login-1", (route) => {
    cancels.push(route.request().method());
    return route.fulfill({ status: 204, body: "" });
  });
  return { cancels, codes };
}

test("keyboard and focus: code field focused, Tab order, focus and value kept after a rejected code, Escape cancels", async ({
  page,
}) => {
  const handle = await installLoginMocks(page);
  await page.goto("/settings/sandbox");
  await page.getByRole("button", { name: "Log in" }).click();

  const dialog = page.getByRole("dialog", { name: "Sandbox login" });
  const code = dialog.getByLabel("Sign-in code");
  await expect(dialog.getByRole("link", { name: /sign in/i })).toHaveAttribute("href", URL_ONE);
  await expect(code).toBeFocused();

  // Shift+Tab walks back to the link, Tab forward reaches the buttons in order.
  await page.keyboard.press("Shift+Tab");
  await expect(dialog.getByRole("button", { name: "Copy link" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(code).toBeFocused();

  await code.fill("bad code");
  await page.keyboard.press("Enter");
  await expect(dialog.getByRole("alert")).toHaveText("The code has a space in it.");
  await expect(code).toBeFocused();
  await expect(code).toHaveValue("bad code");

  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
  await expect.poll(() => handle.cancels).toEqual(["DELETE"]);
  expect(handle.codes).toEqual(["bad code"]);
});

test("a good code closes the dialog and Escape afterwards cancels nothing", async ({ page }) => {
  const handle = await installLoginMocks(page);
  await page.goto("/settings/sandbox");
  await page.getByRole("button", { name: "Log in" }).click();
  const dialog = page.getByRole("dialog", { name: "Sandbox login" });
  await dialog.getByLabel("Sign-in code").fill("good-code");
  await page.keyboard.press("Enter");
  await expect(dialog).toHaveCount(0);
  await page.keyboard.press("Escape");
  expect(handle.codes).toEqual(["good-code"]);
  expect(handle.cancels).toEqual([]);
});
