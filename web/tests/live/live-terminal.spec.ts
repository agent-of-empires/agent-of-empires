// The live terminal view against a real server and tmux: frame transport, compression, size ownership, scrollback.

import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { devices, type Page } from "@playwright/test";
import { test, expect, type ServeHandle, type ServeOptions } from "../helpers/liveTest";
import { seedSessionViaAoeAdd, waitForSessions } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

test.use({ ...devices["iPhone 13"] });

const serveAgent = (spawnServe: (opts?: ServeOptions) => Promise<ServeHandle>, title: string, agentScript: string) =>
  spawnServe({ seedFn: seedSessionViaAoeAdd({ title, agentScript }) });

async function openLiveSession(page: Page, serve: ServeHandle, title: string, query = "") {
  await page.goto(`${serve.baseUrl}/${query}`);
  await openMobileSidebar(page);
  await clickSidebarSession(page, title);
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 15_000 });
}

const liveDebug = (page: Page) =>
  page
    .locator("[data-live-debug]")
    .first()
    .textContent()
    .then((t) => t ?? "");
const reportedTransport = async (page: Page) => /transport=(\w+)/.exec(await liveDebug(page))?.[1] ?? null;

/** Fail if the server fell back to snapshots, which cannot hold a half-drawn repaint. */
async function expectGridTransport(page: Page) {
  await expect
    .poll(() => reportedTransport(page), { timeout: 30_000, message: "server reported which transport is live" })
    .not.toBeNull();
  expect(await reportedTransport(page), "the VT grid armed; a snapshot fallback cannot hold a repaint").toBe("grid");
}

test.describe("grid transport", () => {
  test("synchronized-output brackets publish whole frames only", async ({ page, spawnServe }) => {
    test.setTimeout(90_000);
    // Like Claude Code's fullscreen renderer: each repaint clears, paints A, pauses, paints B inside one 2026 bracket.
    const serve = await serveAgent(
      spawnServe,
      "sync-app",
      `#!/bin/bash
printf '\\e[?1049h\\e[?25l'
i=0
while true; do
  i=$((i+1))
  printf '\\e[?2026h\\e[2J\\e[HFRAME-A %d' "$i"
  sleep 0.06
  printf '\\e[3;1HFRAME-B %d\\e[?2026l' "$i"
  sleep 0.12
done
`,
    );
    await openLiveSession(page, serve, "sync-app", "?livedebug=1");
    await expectGridTransport(page);
    const whole = (text: string | null) => {
      const a = /FRAME-A (\d+)/.exec(text ?? "");
      const b = /FRAME-B (\d+)/.exec(text ?? "");
      return a && b && a[1] === b[1] ? a[1]! : null;
    };
    await expect
      .poll(async () => whole(await page.locator('[data-term="agent"] [data-live-content]').textContent()) !== null, {
        timeout: 30_000,
      })
      .toBe(true);

    // Sample far faster than the app repaints; a torn frame pairs A and B from different repaints.
    const result = await page.evaluate(
      () =>
        new Promise<{ torn: string[]; frames: number }>((resolve) => {
          const torn: string[] = [];
          const seen = new Set<string>();
          let samples = 0;
          const timer = setInterval(() => {
            const text = document.querySelector('[data-term="agent"] [data-live-content]')?.textContent ?? "";
            const a = /FRAME-A (\d+)/.exec(text);
            const b = /FRAME-B (\d+)/.exec(text);
            if (!a || !b || a[1] !== b[1]) torn.push(text.replace(/\s+/g, " ").trim().slice(0, 60));
            else seen.add(a[1]!);
            if (++samples >= 150) {
              clearInterval(timer);
              resolve({ torn, frames: seen.size });
            }
          }, 20);
        }),
    );
    expect(result.frames, "the app kept repainting during the sample window").toBeGreaterThan(3);
    expect(result.torn, "no sample showed a half-drawn repaint").toEqual([]);
    await expect
      .poll(() => reportedTransport(page), { timeout: 30_000, message: "server switched to grid transport" })
      .toBe("grid");
  });

  test("a resize keeps the live grid and its OSC 52 forwarding", async ({ page, context, spawnServe }) => {
    test.setTimeout(90_000);
    const marker = `POST_RESEED_${randomUUID().slice(0, 8)}`;
    // Prints the marker and an OSC 52 copy only after the first input byte.
    const serve = await serveAgent(
      spawnServe,
      "clipboard-fallback",
      `#!/bin/bash
printf 'CLIPBOARD_READY\\n'
IFS= read -r -n 1 _
printf '\\n${marker}\\n'
printf '\\e]52;c;YWZ0ZXItcmVzaXpl\\a'
while true; do sleep 1; done
`,
    );
    const session = (await waitForSessions(serve.baseUrl)).find((s) => s.title === "clipboard-fallback");
    if (!session) throw new Error("clipboard fixture session was not seeded");
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    const socketUrl = new URL(`/sessions/${session.id}/live-ws`, serve.baseUrl);
    socketUrl.protocol = socketUrl.protocol === "https:" ? "wss:" : "ws:";

    type ResizeWindow = typeof window & {
      __liveResize: {
        socket?: WebSocket;
        send: WebSocket["send"];
        connections: number;
        events: Array<{ type: "transport"; grid: boolean } | { type: "close"; code: number }>;
      };
    };
    // Journal the pinned socket and drop client resizes so only the explicit one below applies.
    await page.addInitScript((socketUrl) => {
      const original = WebSocket.prototype.send;
      const state: ResizeWindow["__liveResize"] = { send: original, connections: 0, events: [] };
      (window as ResizeWindow).__liveResize = state;
      const sockets = new WeakSet<WebSocket>();
      const desc = Object.getOwnPropertyDescriptor(WebSocket.prototype, "onmessage")!;
      Object.defineProperty(WebSocket.prototype, "onmessage", {
        configurable: true,
        get() {
          return desc.get!.call(this) as unknown;
        },
        set(this: WebSocket, handler: ((ev: MessageEvent) => void) | null) {
          if (this.url === socketUrl && handler && !sockets.has(this)) {
            sockets.add(this);
            state.connections += 1;
            state.socket ??= this;
            this.addEventListener("close", (event) => state.events.push({ type: "close", code: event.code }));
            this.addEventListener("message", (event) => {
              if (typeof event.data !== "string") return;
              const message = JSON.parse(event.data) as { type?: string; grid: boolean };
              if (message.type === "transport") state.events.push({ type: "transport", grid: message.grid });
            });
          }
          desc.set!.call(this, handler);
        },
      });
      WebSocket.prototype.send = function (data: Parameters<WebSocket["send"]>[0]) {
        if (this.url === socketUrl && typeof data === "string") {
          try {
            if ((JSON.parse(data) as { type?: string }).type === "resize") return;
          } catch {
            // Non-JSON text frames pass through.
          }
        }
        return original.call(this, data);
      };
    }, socketUrl.href);

    const paneGeometry = () => {
      const result = spawnSync(
        "tmux",
        ["-S", serve.tmuxSocket, "list-panes", "-a", "-F", "#{pane_width} #{pane_height}"],
        { env: serve.env, encoding: "utf8" },
      );
      const [cols, rows] = result.stdout.trim().split(/\s+/).map(Number);
      return result.status === 0 && cols && rows ? { cols, rows } : undefined;
    };

    await openLiveSession(page, serve, "clipboard-fallback", "?livedebug=1");
    const terminal = page.locator('[data-term="agent"] [data-live-terminal]');
    await expect(terminal.locator("[data-live-content]")).toContainText("CLIPBOARD_READY", { timeout: 30_000 });
    await expect
      .poll(() => page.evaluate(() => (window as ResizeWindow).__liveResize.events[0]), { timeout: 30_000 })
      .toEqual({ type: "transport", grid: true });
    await page.evaluate(() => navigator.clipboard.writeText(""));
    let geometry: { cols: number; rows: number } | undefined;
    await expect
      .poll(() => (geometry = paneGeometry() ?? geometry), {
        timeout: 15_000,
        message: "isolated tmux pane exposed its geometry",
      })
      .toBeDefined();
    const resized = { cols: geometry!.cols + 1, rows: geometry!.rows + 1 };
    await page.evaluate(({ cols, rows }) => {
      const { socket, send } = (window as ResizeWindow).__liveResize;
      if (!socket || socket.readyState !== WebSocket.OPEN) throw new Error("pinned live WebSocket is not open");
      send.call(socket, JSON.stringify({ type: "resize", cols, rows }));
    }, resized);
    await expect
      .poll(paneGeometry, { timeout: 15_000, message: "tmux applied both resize dimensions" })
      .toEqual(resized);
    // rows comes from the received frame, unlike the client's render column count.
    await expect(terminal.locator("[data-live-debug]")).toContainText(new RegExp(`\\brows=${resized.rows}\\b`), {
      timeout: 30_000,
    });
    await terminal.click();
    await page.keyboard.type("c");
    await expect
      .poll(
        () =>
          terminal.evaluate((root) => ({
            rows: Number(/\brows=(\d+)\b/.exec(root.querySelector("[data-live-debug]")?.textContent ?? "")?.[1]),
            output: root.querySelector("[data-live-content]")?.textContent ?? "",
          })),
        { timeout: 30_000, message: "the resized frame rendered output caused by the later input" },
      )
      .toEqual({ rows: resized.rows, output: expect.stringContaining(marker) });
    await expect
      .poll(() => page.evaluate(() => navigator.clipboard.readText()), {
        timeout: 30_000,
        message: "OSC 52 emitted after the resize reached the same viewer",
      })
      .toBe("after-resize");
    const journal = await page.evaluate(() => {
      const { socket, connections, events } = (window as ResizeWindow).__liveResize;
      return { connections, events, open: socket?.readyState === WebSocket.OPEN };
    });
    expect(journal.connections, "no replacement viewer before the post-resize output and clipboard").toBe(1);
    expect(journal.open, "the pinned viewer is still open").toBe(true);
    expect(
      journal.events.filter((event) => event.type !== "transport" || !event.grid),
      "no close or transient fallback from the initial grid announcement through the witness",
    ).toEqual([]);
  });

  test("a streaming agent is delivered as row patches after the first frame", async ({ page, spawnServe }) => {
    test.setTimeout(90_000);
    const serve = await serveAgent(
      spawnServe,
      "patch-stream",
      `#!/bin/bash\necho "PATCH_READY"\ni=0\nwhile true; do i=$((i+1)); echo "patch line $i"; sleep 0.15; done\n`,
    );
    await openLiveSession(page, serve, "patch-stream", "?livedebug=1");
    await expectGridTransport(page);
    await page
      .locator("[data-live-content]")
      .filter({ hasText: /patch line \d+/ })
      .waitFor({ state: "attached", timeout: 30_000 });

    const counters = async () => {
      const m = /frames=(\d+) patches=(\d+) resyncs=(\d+)/.exec(await liveDebug(page));
      return m ? { frames: Number(m[1]), patches: Number(m[2]), resyncs: Number(m[3]) } : null;
    };
    await expect
      .poll(async () => (await counters())?.patches ?? 0, { timeout: 20_000, message: "row patches arrived" })
      .toBeGreaterThan(3);
    const first = (await counters())!;
    // Each appended line slides the window: a patch carries `shift` plus one row.
    await expect
      .poll(async () => (await counters())?.patches ?? 0, { timeout: 20_000 })
      .toBeGreaterThan(first.patches + 3);
    const later = (await counters())!;
    expect(later.resyncs, "continuity never broke").toBe(0);
    expect(later.frames, "steady streaming did not fall back to full frames").toBeLessThanOrEqual(first.frames + 1);
  });
});

test("live frames arrive compressed (binary) and render through the inflater", async ({ page, spawnServe }) => {
  test.setTimeout(90_000);
  const serve = await serveAgent(
    spawnServe,
    "compression-test",
    `#!/bin/bash\necho "COMPRESS_READY"\ni=0\nwhile true; do i=$((i+1)); echo "compress line $i"; sleep 0.2; done\n`,
  );
  // Count live-ws payload types via the onmessage setter the app uses, and record sent control text.
  await page.addInitScript(() => {
    const w = window as unknown as { __LIVE_WS_FRAMES__: { text: number; binary: number }; __LIVE_WS_SENT__: string[] };
    w.__LIVE_WS_FRAMES__ = { text: 0, binary: 0 };
    w.__LIVE_WS_SENT__ = [];
    const origSend = WebSocket.prototype.send;
    WebSocket.prototype.send = function (this: WebSocket, data: Parameters<WebSocket["send"]>[0]) {
      if (this.url.includes("live-ws") && typeof data === "string") w.__LIVE_WS_SENT__.push(data);
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
              if (this.url.includes("live-ws"))
                w.__LIVE_WS_FRAMES__[typeof ev.data === "string" ? "text" : "binary"] += 1;
              handler.call(this, ev);
            }
          : handler;
        desc.set!.call(this, wrapped);
      },
    });
  });
  await openLiveSession(page, serve, "compression-test");
  // The ready marker scrolls away on a slow worker, so wait for streamed lines.
  await page
    .locator("[data-live-content]")
    .filter({ hasText: /compress line \d+/ })
    .waitFor({ state: "attached", timeout: 30_000 });
  const sent = await page.evaluate(() => (window as unknown as { __LIVE_WS_SENT__: string[] }).__LIVE_WS_SENT__);
  expect(
    sent.some((s) => s.includes('"caps"')),
    "client advertised the deflate capability",
  ).toBe(true);

  // Connections start in text mode and switch once the server processes `caps`, so poll for binary.
  const binaryCount = () =>
    page.evaluate(() => (window as unknown as { __LIVE_WS_FRAMES__: { binary: number } }).__LIVE_WS_FRAMES__.binary);
  await expect
    .poll(binaryCount, { timeout: 15_000, message: "frames arrived as compressed binary messages" })
    .toBeGreaterThan(0);
  const binaryBefore = await binaryCount();
  const lastLine = () =>
    page.evaluate(() => {
      const rows = Array.from(document.querySelectorAll("[data-live-content] > div"));
      for (let i = rows.length - 1; i >= 0; i--) {
        const m = /compress line (\d+)/.exec(rows[i]!.textContent ?? "");
        if (m) return Number(m[1]);
      }
      return 0;
    });
  // Later frames ride the same deflate stream; the counter increments before the app paints.
  const before = await lastLine();
  await expect.poll(lastLine, { timeout: 15_000 }).toBeGreaterThan(before);
  expect(await binaryCount()).toBeGreaterThan(binaryBefore);
});

test("scrollback remains available through the web capture limit", async ({ page, spawnServe }) => {
  test.setTimeout(90_000);
  // 2,500 lines exceed the VT seed cache (2,000) but not the advertised 4,000-line capture.
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "scrollback-test",
      agentScript: `#!/bin/bash\nfor i in $(seq 1 2500); do echo "scrollline $i"; done\necho "PROMPT_READY"\nwhile true; do sleep 1; done\n`,
      prepare: (_dir, env) => {
        // Raise the history limit on the pinned socket before the agent pane exists.
        for (const args of [
          ["new-session", "-d", "-s", "history-bootstrap", "sleep 30"],
          ["set-option", "-g", "history-limit", "4000"],
        ]) {
          const res = spawnSync("tmux", ["-S", env.AOE_TMUX_SOCKET!, ...args], { env });
          if (res.status !== 0) throw new Error(String(res.stderr));
        }
      },
    }),
  });
  await openLiveSession(page, serve, "scrollback-test");
  await page
    .locator("[data-live-content]")
    .filter({ hasText: "PROMPT_READY" })
    .waitFor({ state: "attached", timeout: 30_000 });
  const scroller = page.locator("[data-live-terminal] > div").first();
  // More than one screenful is already loaded above the live edge.
  await expect
    .poll(() =>
      scroller.evaluate((el) => {
        const rows = Array.from(el.querySelectorAll("[data-live-content] > div")) as HTMLElement[];
        const h = rows.length >= 2 ? rows[rows.length - 1]!.getBoundingClientRect().height : 16;
        const nums = rows.flatMap((r) => {
          const n = /scrollline (\d+)/.exec(r.textContent ?? "")?.[1];
          return n ? [Number(n)] : [];
        });
        return nums.length > 0 && Math.max(...nums) - Math.min(...nums) > Math.round(el.clientHeight / h);
      }),
    )
    .toBe(true);

  // The oldest 500 lines must come from tmux via the capture fallback.
  await scroller.evaluate((el) => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll"));
  });
  await expect
    .poll(
      () =>
        scroller.evaluate((el) => {
          const top = el.scrollTop;
          const bottom = top + el.clientHeight;
          return Array.from(el.querySelectorAll("[data-live-content] > div"))
            .filter((row): row is HTMLElement => row instanceof HTMLElement)
            .filter((row) => row.offsetTop >= top && row.offsetTop < bottom)
            .map((row) => row.textContent ?? "")
            .join("|");
        }),
      { timeout: 10_000 },
    )
    .toContain("scrollline 1");
});

test.describe("size ownership", () => {
  const PROMPT = "READY>";
  // Scrollback, then a parked prompt redrawn with a bare CR on SIGWINCH like a real agent.
  const PROMPTBOX = `#!/bin/bash
for i in $(seq 1 30); do echo "line-$i"; done
printf '${PROMPT} '
trap "printf '\\r${PROMPT} '" WINCH
while true; do sleep 1; done
`;
  const ON_PROMPT = { promptRows: 1, cursor: "on the prompt row" };

  async function openPhones(browser: import("@playwright/test").Browser, serve: ServeHandle) {
    const ctxA = await browser.newContext({ ...devices["iPhone 13"] });
    const ctxB = await browser.newContext({ ...devices["iPhone 13"], viewport: { width: 360, height: 740 } });
    const open = async (page: Page) => {
      await openLiveSession(page, serve, "takeover-test");
      await expect.poll(() => page.locator("[data-live-content]").innerText(), { timeout: 15_000 }).toContain(PROMPT);
    };
    return { ctxA, ctxB, a: await ctxA.newPage(), b: await ctxB.newPage(), open };
  }

  /**
   * Prompt row count plus the cursor's offset from it, phrased so a mismatch reports the offset. The WINCH redraw
   * never adds a newline, so a second prompt row means the grid put the cursor on a row the pane never had (#3824).
   */
  const promptAlignment = (page: Page) =>
    page.evaluate((prompt) => {
      const content = document.querySelector("[data-live-content]");
      const cursor = document.querySelector("[data-live-cursor]");
      if (!content || !cursor) return { promptRows: -1, cursor: "no live content" };
      const promptRows = Array.from(content.children).filter(
        (el) => !el.hasAttribute("data-live-cursor") && (el.textContent ?? "").includes(prompt),
      );
      if (!promptRows[0]) return { promptRows: 0, cursor: "no prompt row" };
      const rect = promptRows[0].getBoundingClientRect();
      const delta = cursor.getBoundingClientRect().top - rect.top;
      const offBy = rect.height > 0 ? Math.round(delta / rect.height) : Number.NaN;
      return {
        promptRows: promptRows.length,
        cursor: Math.abs(delta) < 2 ? "on the prompt row" : `${offBy} rows off (${delta.toFixed(1)}px)`,
      };
    }, PROMPT);

  async function takeOver(page: Page) {
    const banner = page.locator("[data-live-takeover]");
    await banner.waitFor({ state: "visible", timeout: 10_000 });
    await banner.click();
    await banner.waitFor({ state: "detached", timeout: 10_000 });
  }

  test("ownership ping-pong keeps the cursor on the prompt row", async ({ browser, spawnServe }) => {
    test.setTimeout(120_000);
    const serve = await serveAgent(spawnServe, "takeover-test", PROMPTBOX);
    const { ctxA, ctxB, a, b, open } = await openPhones(browser, serve);
    try {
      await open(a);
      await expect(a.locator("[data-live-takeover]")).toHaveCount(0);
      await expect.poll(() => promptAlignment(a), { timeout: 10_000 }).toEqual(ON_PROMPT);
      await open(b);
      // The reported bug drifted the cursor a row below the prompt on every take-back.
      for (const [winner, loser] of [
        [b, a],
        [a, b],
        [b, a],
        [a, b],
        [b, a],
      ] as const) {
        await takeOver(winner);
        await loser.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
        await expect.poll(() => promptAlignment(winner), { timeout: 10_000 }).toEqual(ON_PROMPT);
      }
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });

  test("released lock auto-reclaims without another take-over tap", async ({ browser, spawnServe }) => {
    test.setTimeout(120_000);
    const serve = await serveAgent(spawnServe, "takeover-test", PROMPTBOX);
    const { ctxA, ctxB, a, b, open } = await openPhones(browser, serve);
    try {
      await open(a);
      await expect(a.locator("[data-live-takeover]")).toHaveCount(0);
      await open(b);
      await takeOver(b);
      await a.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
      // A visible demoted viewer reclaims the vacant lock on its own.
      await ctxB.close();
      await a.locator("[data-live-takeover]").waitFor({ state: "detached", timeout: 15_000 });
      await expect.poll(() => promptAlignment(a), { timeout: 10_000 }).toEqual(ON_PROMPT);
    } finally {
      await ctxA.close();
      await ctxB.close().catch(() => {});
    }
  });
});
