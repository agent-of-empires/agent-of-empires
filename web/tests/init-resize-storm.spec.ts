import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";

// Regression for #807. useTerminal.ts used to read term.cols/term.rows
// inside ws.onopen, which yields xterm.js's 80x24 default before the
// FitAddon has measured the container. The result was an init-time
// resize storm: client sent 80x24 -> server resized PTY -> SIGWINCH ->
// regular-screen TUI (opencode/Claude) redrew -> previous frame stacked
// into tmux scrollback as garbled output. Fix calls fit() synchronously
// after term.open() so lastMeasuredRef is populated before ws.onopen
// fires, and gates ws.onopen resize sends on that ref so the 80x24
// default never leaves the client.

const desktop = { width: 1280, height: 800 };
test.use({ viewport: desktop, hasTouch: false });

interface ResizeMsg {
  type: "resize";
  cols: number;
  rows: number;
}

function extractResizes(handle: MockHandle): ResizeMsg[] {
  const out: ResizeMsg[] = [];
  for (const msg of handle.wsMessages) {
    const s = msg.toString("utf8");
    if (!s.startsWith("{")) continue;
    try {
      const parsed = JSON.parse(s);
      if (parsed?.type === "resize") out.push(parsed);
    } catch {
      // not json
    }
  }
  return out;
}

async function openSession(page: Page, handle: MockHandle) {
  await clickSidebarSession(page, "pinch-test");
  await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.wsMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
}

test.describe("Init resize storm regression (#807)", () => {
  test("never sends xterm.js's 80x24 default at session open", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);

    // Generous settle window: ResizeObserver, font swap, panel mounts,
    // and the longer initial debounce inside useTerminal all need to
    // resolve before we sample the resize message stream.
    await page.waitForTimeout(1000);

    const resizes = extractResizes(handle);
    expect(resizes.length).toBeGreaterThan(0);

    const default80x24 = resizes.filter((r) => r.cols === 80 && r.rows === 24);
    expect(
      default80x24,
      `Saw ${default80x24.length} resize msgs at xterm.js's 80x24 default. ` +
        `useTerminal must call fit() synchronously after term.open() and ` +
        `gate ws.onopen sends on lastMeasuredRef so the default never ` +
        `reaches the server. Full sequence: ` +
        JSON.stringify(resizes),
    ).toHaveLength(0);
  });

  test("init storm is bounded: small msg count, no duplicate sizes", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const resizes = extractResizes(handle);
    expect(resizes.length).toBeGreaterThan(0);

    // The dashboard mounts two terminals (TerminalView + RightPanel),
    // each with its own useTerminal/WebSocket. With seed + dedup +
    // longer initial debounce, each pane should send exactly one
    // resize during init (its real measured size). Two panes -> 2
    // messages. We allow up to 4 to absorb test-environment timing
    // jitter, but anything higher means the storm has crept back.
    expect(
      resizes.length,
      `Init resize storm not bounded: got ${resizes.length} msgs. ` +
        `Expected <= 4 (one per terminal pane, plus jitter). ` +
        JSON.stringify(resizes),
    ).toBeLessThanOrEqual(4);

    // sendResize dedupes consecutive identical (cols,rows) on the same
    // socket. Both terminals' messages interleave into one captured
    // list, so we can't strictly group by socket. But each pane has a
    // distinct container width, so each unique (cols,rows) should land
    // at most once per pane. The number of distinct sizes is therefore
    // a tight upper bound on total messages: any extras are dedup
    // failures.
    const distinct = new Set(resizes.map((r) => `${r.cols}x${r.rows}`));
    expect(
      resizes.length,
      `Saw ${resizes.length} resize msgs but only ${distinct.size} ` +
        `distinct sizes — dedup must filter consecutive duplicates ` +
        `from the rAF + onResize race. Sequence: ` +
        JSON.stringify(resizes),
    ).toBeLessThanOrEqual(distinct.size);
  });
});
