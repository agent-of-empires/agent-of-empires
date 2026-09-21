// The mobile toolbar's key buttons must put their byte sequences on the live PTY socket.

import { devices } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

test.use({ ...devices["iPhone 13"] });

// `toContain` on an array is element equality, so "Arrow up" cannot satisfy the plain ESC row.
const KEYS: Array<[string, string]> = [
  ["Escape", "\x1b"],
  ["Tab", "\t"],
  ["Ctrl+C interrupt", "\x03"],
  ["Arrow up", "\x1b[A"],
];

test("mobile toolbar buttons send their key sequences", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "story-mobile-keys" }) });
  const [seeded] = await listSessions(serve.baseUrl);

  await page.addInitScript(() => {
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

  for (const [name, bytes] of KEYS) {
    await test.step(`${name} sends its sequence`, async () => {
      const button = page.getByRole("button", { name });
      await expect(button).toBeVisible({ timeout: 15_000 });
      await button.click();
      await expect
        .poll(() => page.evaluate(() => (window as unknown as { __WS_SENT__: string[] }).__WS_SENT__), {
          timeout: 5_000,
        })
        .toContain(bytes);
    });
  }
});
