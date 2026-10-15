// The mobile key row and arrow joystick must put their byte sequences on the live PTY socket.

import { devices, type Page } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

test.use({ ...devices["iPhone 13"] });

// `toContain` on an array is element equality, so an arrow cannot satisfy the plain ESC row.
const KEYS: Array<[string, string]> = [
  ["Escape", "\x1b"],
  ["Tab", "\t"],
  ["Ctrl+C interrupt", "\x03"],
];

const sent = (page: Page) => page.evaluate(() => (window as unknown as WireWindow).__WS_SENT__);
type WireWindow = {
  __WS_SENT__: string[];
  __INPUT_SOCKET__?: WebSocket;
  __INPUT_SEND__: WebSocket["send"];
};

test("mobile toolbar buttons send their key sequences", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "story-mobile-keys",
      agentScript: `#!/bin/bash
set -e
stty raw -echo
printf '\\e[?2004l'
exec python3 -u -c 'import os
f = open(os.path.join(os.environ["HOME"], "received.bin"), "ab", buffering=0)
print("CTRL_CAPTURE_READY", flush=True)
while True:
 b = os.read(0, 1)
 if not b: break
 f.write(b)
 if 32 <= b[0] < 127: os.write(1, b)
'
`,
    }),
  });
  for (const tool of ["python3", "stty"]) {
    const result = spawnSync("bash", ["-c", 'command -v "$1"', "bash", tool], {
      env: serve.env,
      encoding: "utf8",
      timeout: 5_000,
    });
    expect(result.error, tool).toBeUndefined();
    expect(result.status, `required input capture tool: ${tool}`).toBe(0);
  }
  const [seeded] = await listSessions(serve.baseUrl);

  const socketUrl = new URL(`/sessions/${seeded!.id}/live-ws`, serve.baseUrl);
  socketUrl.protocol = "ws:";
  await page.addInitScript((socketUrl) => {
    localStorage.setItem(
      "aoe-web-settings",
      JSON.stringify({
        mobileToolbarKeys: ["ctrl", "enter", "ctrl-c", "backspace", "esc", "tab", "compose"],
      }),
    );
    const w = window as unknown as WireWindow;
    w.__WS_SENT__ = [];
    const origSend = WebSocket.prototype.send;
    w.__INPUT_SEND__ = origSend;
    WebSocket.prototype.send = function (data: BufferSource | string) {
      const result = origSend.call(this, data as never);
      if (this.url !== socketUrl) return result;
      if (typeof data === "string") w.__WS_SENT__.push(data);
      else if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
        w.__WS_SENT__.push(new TextDecoder().decode(data));
        w.__INPUT_SOCKET__ ??= this;
      }
      return result;
    };
  }, socketUrl.href);
  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded!.id)}`);

  await expect(page.locator("[data-live-content]").first()).toContainText("CTRL_CAPTURE_READY", { timeout: 15_000 });
  const file = join(serve.home, "received.bin");
  const ctrl = page.getByRole("button", { name: "Ctrl", exact: true });
  const input = page.getByRole("textbox", { name: "Live terminal input", exact: true });
  let consumed = 0;
  let barriers = 0;
  const received = async () => {
    await expect.poll(() => page.evaluate(() => !!(window as unknown as WireWindow).__INPUT_SOCKET__)).toBe(true);
    await page.evaluate(() => {
      const w = window as unknown as WireWindow;
      if (!w.__INPUT_SOCKET__ || w.__INPUT_SOCKET__.readyState !== WebSocket.OPEN)
        throw new Error("input socket changed");
      w.__INPUT_SEND__.call(w.__INPUT_SOCKET__, new Uint8Array([0x1e]));
    });
    barriers++;
    await expect.poll(() => readFileSync(file).filter((b) => b === 0x1e).length, { timeout: 10_000 }).toBe(barriers);
    const bytes = readFileSync(file);
    const end = bytes.indexOf(0x1e, consumed);
    const segment = bytes.subarray(consumed, end);
    consumed = end + 1;
    return segment;
  };

  await test.step("Ctrl then Enter consumes the modifier without requesting keyboard focus", async () => {
    await expect(input).not.toBeFocused();
    await ctrl.click();
    await page.getByRole("button", { name: "Enter", exact: true }).click();
    const latched = await ctrl.getAttribute("aria-pressed");
    await expect(input).not.toBeFocused();
    expect(await page.evaluate(() => document.activeElement instanceof HTMLTextAreaElement)).toBe(false);
    await page.getByRole("button", { name: "Open keyboard", exact: true }).click();
    await expect(input).toBeFocused();
    await page.keyboard.type("c");
    expect(await received()).toEqual(Buffer.from("\rc"));
    expect(latched).toBe("false");
  });
  await test.step("focused typing stays literal after toolbar keys", async () => {
    for (const latch of [false, true]) {
      if (latch) await ctrl.click();
      await page.getByRole("button", { name: "Enter", exact: true }).click();
      await expect(input).toBeFocused();
      await page.keyboard.type("c");
      expect(await received()).toEqual(Buffer.from("\rc"));
      await expect(ctrl).toHaveAttribute("aria-pressed", "false");
    }
    await ctrl.click();
    await page.getByRole("button", { name: "Ctrl+C interrupt", exact: true }).click();
    await expect(input).toBeFocused();
    await page.keyboard.type("c");
    expect(await received()).toEqual(Buffer.from("\x03c"));
    await expect(ctrl).toHaveAttribute("aria-pressed", "false");
  });
  await test.step("held keys consume Ctrl once and preserve focus", async () => {
    await ctrl.click();
    const back = page.getByRole("button", { name: "Backspace", exact: true });
    const box = (await back.boundingBox())!;
    const start = readFileSync(file).length;
    const cdp = await page.context().newCDPSession(page);
    try {
      await cdp.send("Input.dispatchTouchEvent", {
        type: "touchStart",
        touchPoints: [{ x: box.x + box.width / 2, y: box.y + box.height / 2 }],
      });
      await expect
        .poll(
          () =>
            readFileSync(file)
              .subarray(start)
              .filter((b) => b === 0x7f).length,
        )
        .toBeGreaterThanOrEqual(1);
      await expect(ctrl).toHaveAttribute("aria-pressed", "false");
      await expect
        .poll(
          () =>
            readFileSync(file)
              .subarray(start)
              .filter((b) => b === 0x7f).length,
        )
        .toBeGreaterThanOrEqual(2);
      await expect(ctrl).toHaveAttribute("aria-pressed", "false");
    } finally {
      try {
        await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
      } finally {
        await cdp.detach();
      }
    }
    await expect(input).toBeFocused();
    await page.keyboard.type("c");
    const bytes = await received();
    expect(bytes.at(-1)).toBe(0x63);
    const repeats = bytes.subarray(0, -1);
    expect(repeats.length).toBeGreaterThanOrEqual(2);
    expect(repeats.every((b) => b === 0x7f)).toBe(true);
    await expect(ctrl).toHaveAttribute("aria-pressed", "false");
  });

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
