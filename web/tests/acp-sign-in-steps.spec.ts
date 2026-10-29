// The composer's Not logged in label opens the Agent's sign-in steps.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";
import { copiedBy } from "./helpers/clipboard";

test.use({ permissions: ["clipboard-read", "clipboard-write"] });

test("the keyboard opens the steps, Copy takes the command, Open Terminal opens the host shell", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "story-sign-in",
    initialEvents: [{ AuthStatusUpdated: { status: { kind: "none", label: "Not logged in" } } }],
  });
  const terminals: string[] = [];
  await page.route(/\/api\/sessions\/[^/]+\/(container-)?terminal\?/, (r) => {
    terminals.push(new URL(r.request().url()).pathname.split("/").pop()!);
    return r.fulfill({ json: {} });
  });
  const restarts: string[] = [];
  await page.route("**/api/sessions/*/restart", (r) => {
    restarts.push(r.request().method());
    return r.fulfill({ json: {} });
  });
  await openStructuredSession(page, mock);

  const label = page.getByRole("button", { name: "Not logged in" });
  await label.focus();
  await page.keyboard.press("Enter");
  const steps = page.getByRole("dialog", { name: /isn't signed in/ });
  await expect(steps).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(steps).toBeHidden();
  await expect(label).toBeFocused();
  await page.keyboard.press("Space");
  await expect(steps).toBeVisible();

  const command = await steps.getByTestId("sign-in-command").textContent();
  expect(command).toBe("claude auth login");
  expect(await copiedBy(page, () => steps.getByRole("button", { name: "Copy command" }).click())).toBe(command);

  await steps.getByRole("button", { name: "Open Terminal" }).click();
  await expect(steps).toBeHidden();
  await expect(page.getByText("Shell", { exact: true })).toBeVisible();
  await expect.poll(() => terminals).toContain("terminal");
  expect(terminals).not.toContain("container-terminal");

  await label.click();
  await page
    .getByRole("dialog", { name: /isn't signed in/ })
    .getByRole("button", { name: "Restart Agent" })
    .click();
  await expect.poll(() => restarts).toEqual(["POST"]);
});

for (const [server, about] of [
  ["read-only", { read_only: true }],
  ["CityHall", { cityhall_mode: true }],
] as const) {
  test(`a ${server} server shows only the command`, async ({ page }) => {
    const mock = await mockAcpSession(page, {
      title: "story-sign-in",
      about,
      initialEvents: [{ AuthStatusUpdated: { status: { kind: "none", label: "Not logged in" } } }],
    });
    await openStructuredSession(page, mock);
    await page.getByRole("button", { name: "Not logged in" }).click();
    const steps = page.getByRole("dialog", { name: /isn't signed in/ });
    await expect(steps.getByTestId("sign-in-command")).toHaveText("claude auth login");
    await expect(steps.getByRole("button")).toHaveText(["Copy command"]);
  });
}
