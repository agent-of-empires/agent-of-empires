// The mobile key row and arrow joystick must put their byte sequences on the live PTY socket.

import { devices, type Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

test.use({ ...devices["iPhone 13"] });

// `toContain` on an array is element equality, so an arrow cannot satisfy the plain ESC row.
const KEYS: Array<[string, string]> = [
  ["Escape", "\x1b"],
  ["Tab", "\t"],
  ["Ctrl+C interrupt", "\x03"],
];

const sent = (page: Page) => page.evaluate(() => (window as unknown as { __WS_SENT__: string[] }).__WS_SENT__);

test("mobile toolbar buttons send their key sequences", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "story-mobile-keys" }) });
  const [seeded] = await listSessions(serve.baseUrl);

  await page.addInitScript(() => {
    // ^C is not in the default row.
    localStorage.setItem(
      "aoe-web-settings",
      JSON.stringify({ mobileToolbarKeys: ["esc", "tab", "ctrl-c", "compose"] }),
    );
    const w = window as unknown as { __WS_SENT__: string[] };
    w.__WS_SENT__ = [];
    const origSend = WebSocket.prototype.send;
    WebSocket.prototype.send = function (data: BufferSource | string) {
      if (typeof data === "string") w.__WS_SENT__.push(data);
      else if (data instanceof ArrayBuffer || ArrayBuffer.isView(data))
        w.__WS_SENT__.push(new TextDecoder().decode(data));
      return origSend.call(this, data as never);
    };
  });
  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded!.id)}`);

  // ^C below ends the pane, so the paste round trip runs first.
  await test.step("compose Insert pastes through tmux into the pane", async () => {
    await page.getByRole("button", { name: "Compose", exact: true }).click();
    await page.getByRole("textbox", { name: "Message" }).fill("aoe-compose-marker");
    await page.getByRole("button", { name: "Insert", exact: true }).click();
    await expect(page.locator("[data-live-content]").first()).toContainText("aoe-compose-marker", { timeout: 10_000 });
  });

  await test.step("dragging the joystick up sends Arrow up", async () => {
    const pad = page.getByRole("group", { name: "Arrow keys joystick" });
    await expect(pad).toBeVisible({ timeout: 15_000 });
    const box = (await pad.boundingBox())!;
    const cx = box.x + box.width / 2;
    const cy = box.y + box.height / 2;
    const cdp = await page.context().newCDPSession(page);
    const touch = (type: string, y: number) =>
      cdp.send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x: cx, y }] } as never);
    await touch("touchStart", cy);
    for (const dy of [6, 14, 24]) await touch("touchMove", cy - dy);
    await touch("touchEnd", cy - 24);
    await expect.poll(() => sent(page), { timeout: 5_000 }).toContain("\x1b[A");
  });

  for (const [name, bytes] of KEYS) {
    await test.step(`${name} sends its sequence`, async () => {
      const button = page.getByRole("button", { name, exact: true });
      await expect(button).toBeVisible({ timeout: 15_000 });
      await button.click();
      await expect.poll(() => sent(page), { timeout: 5_000 }).toContain(bytes);
    });
  }
});
