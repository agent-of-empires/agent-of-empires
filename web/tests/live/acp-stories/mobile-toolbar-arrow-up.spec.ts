// User story: tapping the Arrow up button on the mobile terminal
// toolbar sends the ANSI up-arrow escape sequence to the PTY.
//
// Patches WebSocket.prototype.send to capture every payload, then
// decodes the binary frames and asserts the escape "\x1b[A" is in
// the trace.

import { test as base, expect, devices } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base.use({ ...devices["iPhone 13"] });

base("mobile toolbar Arrow up sends ANSI up-arrow", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-mobile-arrow-up" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-mobile-arrow-up");
    if (!seeded) throw new Error("seeded session 'story-mobile-arrow-up' missing");
    const sessionId = seeded.id;

    await page.addInitScript(() => {
      const w = window as unknown as { __WS_SENT__: string[] };
      w.__WS_SENT__ = [];
      const origSend = WebSocket.prototype.send;
      WebSocket.prototype.send = function (data: BufferSource | string) {
        try {
          if (data instanceof ArrayBuffer) {
            w.__WS_SENT__.push(new TextDecoder().decode(new Uint8Array(data)));
          } else if (ArrayBuffer.isView(data)) {
            w.__WS_SENT__.push(
              new TextDecoder().decode(data as unknown as Uint8Array),
            );
          } else if (typeof data === "string") {
            w.__WS_SENT__.push(data);
          }
        } catch {
          // swallow
        }
        return origSend.call(this, data as never);
      };
    });

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    const arrowUp = page.getByRole("button", { name: "Arrow up" });
    await expect(arrowUp).toBeVisible({ timeout: 15_000 });
    await arrowUp.click();

    await expect
      .poll(
        async () =>
          await page.evaluate(
            () => (window as unknown as { __WS_SENT__: string[] }).__WS_SENT__,
          ),
        { timeout: 5_000 },
      )
      .toContain("\x1b[A");
  } finally {
    await serve.stop();
  }
});
