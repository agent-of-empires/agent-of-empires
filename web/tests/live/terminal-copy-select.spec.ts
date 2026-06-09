// Live spec for issue #1499: select-to-copy in the xterm.js web terminal.
//
// Before the wterm -> xterm.js swap a plain drag-select landed text on the
// clipboard with no modifier. After the swap it silently failed: tmux owns
// the selection (mouse-mode is on) and emits an OSC 52 clipboard escape on
// copy, but xterm.js had no OSC 52 handler so the payload was dropped. The
// hook now registers a handler that writes the payload to the system
// clipboard, bridging the async gesture gap with a ClipboardItem promise.
//
// This drives the full round-trip against a real backend: a fake `claude`
// shim floods the pane with recognizable marker lines, then a mouse drag in
// the browser selects text and we assert it reached the clipboard. tmux's
// native mouse-drag-selection is the only path that selects across the
// scrollback boundary, so this also exercises the "select above the visible
// area" flow when the drag spans multiple rows.

import { test as base, expect } from "@playwright/test";
import { writeFileSync, chmodSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

const MARKER = "AOECOPY";

base("plain drag-select copies terminal text to the clipboard", async ({ page, context }, testInfo) => {
  const addSession = seedSessionViaAoeAdd({ title: "copytest" });
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: async (env) => {
      // Replace the tail-f-dev-null stub with a shim that prints enough
      // marker lines to overflow the pane (so scrollback exists), then
      // holds the pane open. Runs before `aoe add` so the session's pane
      // starts with this content already on screen.
      const shim = join(env.shimBin, "claude");
      writeFileSync(
        shim,
        '#!/bin/bash\nfor i in $(seq 1 80); do echo "' +
          MARKER +
          '$i line of terminal output"; done\nexec tail -f /dev/null\n',
      );
      chmodSync(shim, 0o755);
      await addSession(env);
    },
  });

  try {
    await context.grantPermissions(["clipboard-read", "clipboard-write"], {
      origin: serve.baseUrl,
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "copytest");
    expect(seeded).toBeTruthy();
    const sessionId: string = seeded!.id;

    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "copytest" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();
    await expect(page).toHaveURL(new RegExp(`/session/${sessionId}`), {
      timeout: 10_000,
    });

    const term = page.locator(".xterm").first();
    await expect(term).toBeVisible({ timeout: 10_000 });

    // Drag-select a row, then read the clipboard. Retry the whole gesture:
    // the pane content arrives asynchronously over the WS and the OSC 52
    // round-trips after mouseup, so the first attempt can land before
    // either is ready. The selection is monotonic for our purposes (we only
    // need one attempt to put a marker on the clipboard).
    type Box = { x: number; y: number; width: number; height: number };
    const dragAndRead = async (toY: (box: Box) => number, steps: number) => {
      const box = await term.boundingBox();
      if (!box) return "";
      await page.evaluate(() => navigator.clipboard.writeText(""));
      const startX = box.x + Math.min(12, box.width * 0.1);
      const startY = box.y + box.height * 0.7;
      await page.mouse.move(startX, startY);
      await page.mouse.down();
      await page.mouse.move(box.x + box.width * 0.85, toY(box), {
        steps,
      });
      await page.mouse.up();
      // Give the SGR drag-end -> tmux copy-selection -> OSC 52 -> WS ->
      // clipboard round-trip a beat to settle before reading.
      await page.waitForTimeout(150);
      return page.evaluate(() => navigator.clipboard.readText());
    };

    // Single-row horizontal drag: the simplest select-to-copy.
    await expect
      .poll(async () => dragAndRead((b) => b.y + b.height * 0.7, 8), {
        timeout: 25_000,
        intervals: [500],
      })
      .toContain(MARKER);

    // Multi-row drag (start low, release above the top edge): tmux extends
    // the copy-mode selection across rows and into scrollback. The copied
    // text should span more than one line and still carry a marker.
    const multiline = await dragAndRead((b) => b.y - 40, 12);
    expect(multiline).toContain(MARKER);
    expect(multiline.split("\n").length).toBeGreaterThan(1);
  } finally {
    await serve.stop();
  }
});
