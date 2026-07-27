// The live view's compressed frame stream, end to end: the client
// advertises `caps.deflate` (Chromium has DecompressionStream), the real
// `aoe serve` switches frame delivery to the sync-flushed raw-deflate
// binary stream, and the browser inflates it back into rendered terminal
// rows. WebSocket messages are instrumented so the test can assert the
// frames genuinely arrived as binary (the compressed path), not text,
// while a streaming agent keeps painting new lines through the same
// stream (dictionary continuity across frames).
import { devices } from "@playwright/test";
import { join } from "node:path";
import { writeFileSync, chmodSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

test("live frames arrive compressed (binary) and render through the inflater", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: (e) => {
      const tool = join(e.shimBin, "streamer");
      writeFileSync(
        tool,
        `#!/bin/bash
echo "COMPRESS_READY"
i=0
while true; do i=$((i+1)); echo "compress line $i"; sleep 0.2; done
`,
      );
      chmodSync(tool, 0o755);
      const pd = join(e.home, "project");
      mkdirSync(pd, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: pd });
      const r = spawnSync(
        resolveAoeBinary(),
        ["add", pd, "-t", "compression-test", "-c", "claude", "--cmd-override", tool],
        { env: e.env },
      );
      if (r.status !== 0) throw new Error(String(r.stderr));
    },
  });
  try {
    const ctx = await browser.newContext({ ...devices["iPhone 13"] });
    const page = await ctx.newPage();
    // Count live-ws message payload types without disturbing delivery. The
    // hook assigns `ws.onmessage`, so wrapping the prototype setter sees
    // every message the app sees.
    await page.addInitScript(() => {
      const counts = { text: 0, binary: 0 };
      const sent: string[] = [];
      (window as unknown as Record<string, unknown>).__LIVE_WS_FRAMES__ = counts;
      (window as unknown as Record<string, unknown>).__LIVE_WS_SENT__ = sent;
      const origSend = WebSocket.prototype.send;
      WebSocket.prototype.send = function (this: WebSocket, data: Parameters<WebSocket["send"]>[0]) {
        if (this.url.includes("live-ws") && typeof data === "string") sent.push(data);
        return origSend.call(this, data);
      };
      const desc = Object.getOwnPropertyDescriptor(WebSocket.prototype, "onmessage")!;
      Object.defineProperty(WebSocket.prototype, "onmessage", {
        configurable: true,
        get() {
          return desc.get!.call(this) as unknown;
        },
        set(this: WebSocket, handler: ((ev: MessageEvent) => void) | null) {
          const wrapped = handler
            ? (ev: MessageEvent) => {
                if (this.url.includes("live-ws")) {
                  if (typeof ev.data === "string") counts.text += 1;
                  else counts.binary += 1;
                }
                handler.call(this, ev);
              }
            : handler;
          desc.set!.call(this, wrapped);
        },
      });
    });
    await page.goto(serve.baseUrl);
    await openMobileSidebar(page);
    await clickSidebarSession(page, "compression-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
    // Wait on the STREAMING lines, not the COMPRESS_READY marker: the shim
    // keeps printing, so the marker scrolls out of the capture window before
    // this assertion can run on a slow worker.
    await page
      .locator("[data-live-content]")
      .filter({ hasText: /compress line \d+/ })
      .waitFor({ state: "attached", timeout: 30_000 });

    // The rendered marker proves at least one frame decoded end to end; now
    // pin down that the frames actually traveled the compressed path.
    const counts = await page.evaluate(
      () => (window as unknown as { __LIVE_WS_FRAMES__: { text: number; binary: number } }).__LIVE_WS_FRAMES__,
    );
    const sent = await page.evaluate(() => (window as unknown as { __LIVE_WS_SENT__: string[] }).__LIVE_WS_SENT__);
    expect(
      sent.some((s) => s.includes('"caps"')),
      "client advertised the deflate capability",
    ).toBe(true);
    expect(counts.binary, "frames arrived as compressed binary messages").toBeGreaterThan(0);

    // The agent keeps streaming; later frames ride the SAME deflate stream
    // (dictionary continuity), so new content must keep rendering and the
    // binary count must keep climbing.
    const lastLine = () =>
      page.evaluate(() => {
        const rows = Array.from(document.querySelectorAll("[data-live-content] > div"));
        for (let i = rows.length - 1; i >= 0; i--) {
          const m = /compress line (\d+)/.exec(rows[i]!.textContent ?? "");
          if (m) return Number(m[1]);
        }
        return 0;
      });
    const before = await lastLine();
    await expect.poll(lastLine, { timeout: 15_000 }).toBeGreaterThan(before);
    const countsLater = await page.evaluate(
      () => (window as unknown as { __LIVE_WS_FRAMES__: { text: number; binary: number } }).__LIVE_WS_FRAMES__,
    );
    expect(countsLater.binary).toBeGreaterThan(counts.binary);
  } finally {
    await serve.stop();
  }
});
