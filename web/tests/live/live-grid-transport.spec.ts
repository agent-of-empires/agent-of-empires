// The agent surface's live view against a real `aoe serve`: frames come from
// the in-process VT grid, so a full-screen app that brackets its repaints in
// DEC 2026 synchronized output is never shown half-drawn, and once the client
// advertises `caps.patch` the stream carries row patches instead of whole
// windows.
//
// Only the first of those is grid-only. Row patches are planned in the shared
// publish path and arrive on the snapshot fallback too, so neither assertion
// proves which transport is live. The server says so directly instead, and
// both cases check it: a run that quietly fell back to snapshots would
// otherwise read as a tearing bug in the grid rather than as an absent grid.
import { devices, type Page } from "@playwright/test";
import { join } from "node:path";
import { writeFileSync, chmodSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary, type ServeHandle, type SpawnOptions } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

/** The server announces its transport on the first frame. Fail here rather
 *  than letting a snapshot fallback masquerade as a grid that tears: the
 *  fallback cannot suppress a half-drawn repaint and is not what these cases
 *  are about. */
async function expectGridTransport(page: Page) {
  const transport = async () => {
    const text = (await page.locator("[data-live-debug]").textContent()) ?? "";
    return /transport=(\w+)/.exec(text)?.[1] ?? null;
  };
  await expect
    .poll(transport, {
      timeout: 30_000,
      message: "server reported which transport is live",
    })
    .not.toBeNull();
  expect(await transport(), "the VT grid armed; a snapshot fallback cannot hold a repaint").toBe("grid");
}

async function expectTransport(page: Page, expected: "grid" | "snapshot") {
  await expect
    .poll(
      async () => {
        const text = (await page.locator("[data-live-debug]").textContent()) ?? "";
        return /transport=(\w+)/.exec(text)?.[1] ?? null;
      },
      { timeout: 30_000, message: `server switched to ${expected} transport` },
    )
    .toBe(expected);
}

async function paneGeometry(serve: ServeHandle): Promise<{ cols: number; rows: number }> {
  let geometry: { cols: number; rows: number } | undefined;
  await expect
    .poll(
      () => {
        const result = spawnSync(
          "tmux",
          ["-S", serve.tmuxSocket, "list-panes", "-a", "-F", "#{pane_width} #{pane_height}"],
          { env: serve.env, encoding: "utf8" },
        );
        const [cols, rows] = result.stdout.trim().split(/\s+/).map(Number);
        if (result.status === 0 && cols && rows) geometry = { cols, rows };
        return geometry;
      },
      { timeout: 15_000, message: "isolated tmux pane exposed its geometry" },
    )
    .toBeDefined();
  return geometry!;
}

async function holdClientResize(page: Page) {
  await page.addInitScript(() => {
    const original = WebSocket.prototype.send;
    (window as Window & { __pinLiveResize?: boolean }).__pinLiveResize = true;
    WebSocket.prototype.send = function (data: string | ArrayBufferLike | Blob | ArrayBufferView) {
      (window as Window & { __liveTestSocket?: WebSocket }).__liveTestSocket = this;
      if (typeof data === "string") {
        try {
          const message = JSON.parse(data) as { type?: string };
          if (message.type === "resize" && (window as Window & { __pinLiveResize?: boolean }).__pinLiveResize) {
            return;
          }
        } catch {
          // Non-control text frames pass through unchanged.
        }
      }
      return original.call(this, data);
    };
  });
}

function seedTool(title: string, script: string): SpawnOptions["seedFn"] {
  return (e) => {
    const tool = join(e.shimBin, `${title}-tool`);
    writeFileSync(tool, script);
    chmodSync(tool, 0o755);
    const pd = join(e.home, "project");
    mkdirSync(pd, { recursive: true });
    spawnSync("git", ["init", "-q"], { cwd: pd });
    const r = spawnSync(resolveAoeBinary(), ["add", pd, "-t", title, "-c", "claude", "--cmd-override", tool], {
      env: e.env,
    });
    if (r.status !== 0) throw new Error(String(r.stderr));
  };
}

// A full-screen app in the style of Claude Code's fullscreen renderer: each
// repaint clears the alternate screen, paints part A, pauses, paints part B
// with the same frame number, all inside one synchronized-output bracket.
const SYNC_APP = `#!/bin/bash
printf '\\e[?1049h\\e[?25l'
i=0
while true; do
  i=$((i+1))
  printf '\\e[?2026h\\e[2J\\e[HFRAME-A %d' "$i"
  sleep 0.06
  printf '\\e[3;1HFRAME-B %d\\e[?2026l' "$i"
  sleep 0.12
done
`;

const STREAMER = `#!/bin/bash
echo "PATCH_READY"
i=0
while true; do i=$((i+1)); echo "patch line $i"; sleep 0.15; done
`;

const CLIPBOARD_AFTER_INPUT = `#!/bin/bash
printf 'CLIPBOARD_READY\\n'
IFS= read -r -n 1 _
printf '\\e]52;c;YWZ0ZXItcmVzaXpl\\a'
while true; do sleep 1; done
`;

test("synchronized-output brackets publish whole frames only", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTool("sync-app", SYNC_APP),
  });
  try {
    const ctx = await browser.newContext({ ...devices["iPhone 13"] });
    const page = await ctx.newPage();
    await holdClientResize(page);
    await page.goto(`${serve.baseUrl}/?livedebug=1`);
    await openMobileSidebar(page);
    await clickSidebarSession(page, "sync-app");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
    await expectGridTransport(page);
    await page
      .locator("[data-live-content]")
      .filter({ hasText: /FRAME-B \d+/ })
      .waitFor({ state: "attached", timeout: 30_000 });

    // Sample the rendered grid far more often than the app repaints. A torn
    // frame shows part A of one repaint with part B of the previous one (or
    // none at all); a whole frame always pairs them.
    const result = await page.evaluate(
      () =>
        new Promise<{ samples: number; torn: string[]; frames: number }>((resolve) => {
          const torn: string[] = [];
          const seen = new Set<string>();
          let samples = 0;
          const timer = setInterval(() => {
            const text = document.querySelector("[data-live-content]")?.textContent ?? "";
            const a = /FRAME-A (\d+)/.exec(text);
            const b = /FRAME-B (\d+)/.exec(text);
            if (!a) return;
            samples += 1;
            seen.add(a[1]!);
            if (!b || a[1] !== b[1]) torn.push(text.replace(/\s+/g, " ").trim().slice(0, 60));
            if (samples >= 150) {
              clearInterval(timer);
              resolve({ samples, torn, frames: seen.size });
            }
          }, 20);
        }),
    );
    expect(result.frames, "the app kept repainting during the sample window").toBeGreaterThan(3);
    expect(result.torn, "no sample showed a half-drawn repaint").toEqual([]);
    await expectTransport(page, "grid");
  } finally {
    await serve.stop();
  }
});

test("resize fallback keeps OSC 52 forwarding on the same WebSocket", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTool("clipboard-fallback", CLIPBOARD_AFTER_INPUT),
  });
  try {
    const ctx = await browser.newContext({ ...devices["iPhone 13"] });
    await ctx.grantPermissions(["clipboard-read", "clipboard-write"]);
    const page = await ctx.newPage();
    await holdClientResize(page);
    await page.goto(`${serve.baseUrl}/?livedebug=1`);
    await openMobileSidebar(page);
    await clickSidebarSession(page, "clipboard-fallback");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
    await expectGridTransport(page);
    const geometry = await paneGeometry(serve);
    await page.evaluate(() => {
      (window as Window & { __pinLiveResize?: boolean }).__pinLiveResize = false;
    });

    await page.evaluate(({ cols, rows }) => {
      const socket = (window as Window & { __liveTestSocket?: WebSocket }).__liveTestSocket;
      if (!socket) throw new Error("live WebSocket was not captured");
      socket.send(JSON.stringify({ type: "resize", cols: cols + 1, rows }));
    }, geometry);
    await expectTransport(page, "snapshot");

    await page.locator("[data-live-terminal]").click();
    await page.keyboard.type("c");
    await expect
      .poll(() => page.evaluate(() => navigator.clipboard.readText()), {
        timeout: 30_000,
        message: "OSC 52 emitted after retirement reached the same viewer",
      })
      .toBe("after-resize");
  } finally {
    await serve.stop();
  }
});

test("a streaming agent is delivered as row patches after the first frame", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTool("patch-stream", STREAMER),
  });
  try {
    const ctx = await browser.newContext({ ...devices["iPhone 13"] });
    const page = await ctx.newPage();
    await holdClientResize(page);
    await page.goto(`${serve.baseUrl}/?livedebug=1`);
    await openMobileSidebar(page);
    await clickSidebarSession(page, "patch-stream");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
    await expectGridTransport(page);
    await page
      .locator("[data-live-content]")
      .filter({ hasText: /patch line \d+/ })
      .waitFor({ state: "attached", timeout: 30_000 });

    const counters = () =>
      page.evaluate(() => {
        const text = document.querySelector("[data-live-debug]")?.textContent ?? "";
        const m = /frames=(\d+) patches=(\d+) resyncs=(\d+)/.exec(text);
        return m ? { frames: Number(m[1]), patches: Number(m[2]), resyncs: Number(m[3]) } : null;
      });
    await expect
      .poll(async () => (await counters())?.patches ?? 0, { timeout: 20_000, message: "row patches arrived" })
      .toBeGreaterThan(3);
    const first = (await counters())!;
    // The agent appends a line every 150 ms; each append slides the window by
    // one row, and a patch carries that as `shift` plus the one new row.
    await expect
      .poll(async () => (await counters())?.patches ?? 0, { timeout: 20_000 })
      .toBeGreaterThan(first.patches + 3);
    const later = (await counters())!;
    expect(later.resyncs, "continuity never broke").toBe(0);
    expect(later.frames, "steady streaming did not fall back to full frames").toBeLessThanOrEqual(first.frames + 1);
  } finally {
    await serve.stop();
  }
});
