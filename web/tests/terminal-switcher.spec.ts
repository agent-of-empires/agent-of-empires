// Terminal tabs bind to one Session's paired shell and stay as the selection changes; the switcher reaches any live
// shell. Layout and binding logic: paneLayout.test.ts; the list: pairedShells.test.ts, TerminalSwitcher.test.tsx.

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockSessionShellApis, mockStaticApis } from "./helpers/apiMocks";
import { mockSessionsList } from "./helpers/sessions";
import { clickSidebarSession } from "./helpers/sidebar";

const alive = (index: number) => ({ target: { kind: "host", index }, state: "alive" });

async function boot(page: Page, width = 1280, path = "/") {
  await mockStaticApis(page);
  await mockSessionShellApis(page);
  await page.route(/\/api\/sessions\/[^/]+\/(container-)?terminal(\?.*)?$/, (r) =>
    r.fulfill({ status: 200, body: "" }),
  );
  await mockSessionsList(page, () => [
    { id: "alpha", title: "alpha", auxiliary: [alive(0)] },
    { id: "beta", title: "beta", auxiliary: [alive(0), alive(2)] },
  ]);
  const sockets: string[] = [];
  page.on("websocket", (ws) => sockets.push(new URL(ws.url()).pathname + new URL(ws.url()).search));
  await page.setViewportSize({ width, height: 720 });
  await page.goto(path);
  return sockets;
}

const pairedSocket = (sockets: string[], session: string, index: number) =>
  sockets.some((url) => url === `/sessions/${session}/terminal/live-ws?index=${index}`);

test("a terminal tab keeps its Session's shell across a sidebar switch, and the switcher opens another's", async ({
  page,
}) => {
  const sockets = await boot(page);
  await clickSidebarSession(page, "alpha");
  await page.getByTestId("pane-toggle-terminal").click();
  const alphaTab = page.getByTestId("pane-tab-terminal:alpha:0");
  await expect(alphaTab).toContainText("alpha · 1");
  await expect.poll(() => pairedSocket(sockets, "alpha", 0)).toBe(true);

  await clickSidebarSession(page, "beta");
  await expect(page).toHaveURL(/\/session\/beta$/);
  await expect(alphaTab).toBeVisible();
  await expect(alphaTab).toHaveAttribute("aria-selected", "true");
  await expect(page.getByTestId("pane-tab-terminal:beta:0")).toHaveCount(0);
  // The tab still shows alpha's shell: nothing connected to beta's.
  expect(sockets.some((url) => url.startsWith("/sessions/beta/terminal/"))).toBe(false);

  await page.getByTestId("terminal-switcher-toggle").click();
  const menu = page.getByTestId("terminal-switcher-menu");
  // The selected Session lists first.
  await expect(menu.getByRole("group")).toHaveText([/^beta/, /^alpha/]);
  await expect(menu.getByTestId("terminal-switcher-item-alpha-0-host")).toContainText("open");
  await expect(menu.getByTestId("terminal-switcher-item-beta-2-host")).toBeVisible();
  await expect(menu.getByTestId("terminal-switcher-new")).toHaveText("New terminal for beta");

  await menu.getByTestId("terminal-switcher-item-beta-2-host").click();
  await expect(menu).toBeHidden();
  const betaTab = page.getByTestId("pane-tab-terminal:beta:2");
  await expect(betaTab).toHaveAttribute("aria-selected", "true");
  await expect(betaTab).toContainText("beta · 3");
  await expect(alphaTab).toBeVisible();
  await expect.poll(() => pairedSocket(sockets, "beta", 2)).toBe(true);

  // The keyboard switcher picks alpha's open tab back.
  await page.keyboard.press("ControlOrMeta+Shift+Backquote");
  await page.getByTestId("terminal-switcher-menu").getByTestId("terminal-switcher-item-alpha-0-host").click();
  await expect(alphaTab).toHaveAttribute("aria-selected", "true");
});

test("on a phone the panel picker opens another Session's live shell", async ({ page }) => {
  const sockets = await boot(page, 390, "/session/alpha");
  await page.getByRole("button", { name: "Toggle panels" }).click();
  const pick = page.getByTestId("mobile-shell-pick-beta-2-host");
  await expect(pick).toContainText("beta · 3");
  await pick.click();
  await expect(page.getByTestId("mobile-paired-layer")).toBeVisible();
  await expect(page.getByTestId("mobile-back-to-agent").locator("..")).toContainText("beta · 3");
  await expect.poll(() => pairedSocket(sockets, "beta", 2)).toBe(true);
});
