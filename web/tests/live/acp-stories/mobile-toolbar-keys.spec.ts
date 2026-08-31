// User story: the mobile terminal toolbar's key buttons reach the PTY.
//
// Patches `WebSocket.prototype.send` to capture every payload, decodes the
// binary frames, and asserts each button puts its byte sequence on the wire.
//
// One spec per button meant four `aoe serve` boots, four tmux sessions and
// four mobile browser contexts to assert four constants: the files differed
// only in the (button name, expected bytes) pair, took the same
// MobileTerminalToolbar -> live-terminal WS path, and always failed together.
// `MobileTerminalToolbar.test.tsx` and `tests/mobile-toolbar.spec.ts` cover
// the buttons' presence, but neither asserts what they emit, so the mapping
// still needs a live PTY behind it.

import { test as base, expect, devices } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base.use({ ...devices["iPhone 13"] });

/** Accessible button name -> the exact payload it must put on the wire.
 *  `toContain` on the captured array is element equality, not substring, so
 *  "Arrow up" does not satisfy the plain ESC row. */
const KEYS: Array<[string, string]> = [
  ["Escape", "\x1b"],
  ["Tab", "\t"],
  ["Ctrl+C interrupt", "\x03"],
  ["Arrow up", "\x1b[A"],
];

base("mobile toolbar buttons send their key sequences", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-mobile-keys" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-mobile-keys");
    if (!seeded) throw new Error("seeded session 'story-mobile-keys' missing");

    await page.addInitScript(() => {
      const w = window as unknown as { __WS_SENT__: string[] };
      w.__WS_SENT__ = [];
      const origSend = WebSocket.prototype.send;
      WebSocket.prototype.send = function (data: BufferSource | string) {
        try {
          if (data instanceof ArrayBuffer) {
            w.__WS_SENT__.push(new TextDecoder().decode(new Uint8Array(data)));
          } else if (ArrayBuffer.isView(data)) {
            w.__WS_SENT__.push(new TextDecoder().decode(data as unknown as Uint8Array));
          } else if (typeof data === "string") {
            w.__WS_SENT__.push(data);
          }
        } catch {
          // swallow
        }
        return origSend.call(this, data as never);
      };
    });

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded.id)}`);

    for (const [name, bytes] of KEYS) {
      await base.step(`${name} sends its sequence`, async () => {
        const button = page.getByRole("button", { name });
        await expect(button).toBeVisible({ timeout: 15_000 });
        await button.click();
        await expect
          .poll(async () => await page.evaluate(() => (window as unknown as { __WS_SENT__: string[] }).__WS_SENT__), {
            timeout: 5_000,
          })
          .toContain(bytes);
      });
    }
  } finally {
    await serve.stop();
  }
});
