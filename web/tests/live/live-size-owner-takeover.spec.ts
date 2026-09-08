// Size-owner take-over round-trip on the mobile live view, against a real
// `aoe serve` + tmux. Two emulated phones ping-pong ownership of one
// session and we assert, after every hand-off, that (a) the loser shows
// the take-over banner and the winner doesn't, and (b) the winner's cursor
// overlay sits on the row that actually contains the agent's prompt, the
// regression reported as "cursor one row below the input box after taking
// control back".

import { devices, type Page } from "@playwright/test";
import { join } from "node:path";
import { writeFileSync, chmodSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

const PROMPT = "READY>";

/** Open the seeded session's live view on an emulated phone page. */
async function openLiveView(page: Page, baseUrl: string) {
  await page.goto(baseUrl);
  await openMobileSidebar(page);
  await clickSidebarSession(page, "takeover-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
  await expect.poll(() => page.locator("[data-live-content]").innerText(), { timeout: 15_000 }).toContain(PROMPT);
}

/** How many rendered rows carry the prompt, and where the cursor overlay sits
 *  relative to the first of them, as a phrase rather than a boolean: an
 *  asserted-equal object reports its mismatched value, so the offset that
 *  identifies the fault survives into the failure message.
 *
 *  The count is asserted, not navigated around. The fixture's WINCH handler
 *  redraws with a bare carriage return and never a newline, so the pane cannot
 *  hold two prompt rows; a second one means the grid put the cursor on a row the
 *  pane never had and the redraw landed there (#3824).
 *
 *  This supersedes #3826, which read that second row as a pre-resize prompt left
 *  behind in the scrollback and measured against the last match instead. It is
 *  not scrollback: at the failing checkpoint tmux reports history_size=0 and a
 *  single prompt row while the grid shows two. Measuring against the last match
 *  made this spec pass with the grid-side duplicate still live. */
async function promptAlignment(page: Page): Promise<{ promptRows: number; cursor: string }> {
  return page.evaluate((prompt) => {
    const content = document.querySelector("[data-live-content]");
    const cursor = document.querySelector("[data-live-cursor]");
    if (!content || !cursor) return { promptRows: -1, cursor: "no live content" };
    const rows = Array.from(content.children).filter((el) => !el.hasAttribute("data-live-cursor"));
    const promptRows = rows.filter((el) => (el.textContent ?? "").includes(prompt));
    const promptRow = promptRows[0];
    if (!promptRow) return { promptRows: 0, cursor: "no prompt row" };
    const rect = promptRow.getBoundingClientRect();
    const delta = cursor.getBoundingClientRect().top - rect.top;
    const offBy = rect.height > 0 ? Math.round(delta / rect.height) : Number.NaN;
    return {
      promptRows: promptRows.length,
      cursor: Math.abs(delta) < 2 ? "on the prompt row" : `${offBy} rows off (${delta.toFixed(1)}px)`,
    };
  }, PROMPT);
}

const ON_PROMPT = { promptRows: 1, cursor: "on the prompt row" };

async function takeOver(page: Page) {
  const banner = page.locator("[data-live-takeover]");
  await banner.waitFor({ state: "visible", timeout: 10_000 });
  // click (not tap) so this works on both touch and fine-pointer contexts now
  // that the live view renders on desktop too.
  await banner.click();
  await banner.waitFor({ state: "detached", timeout: 10_000 });
}

/** Seed one session running a fake agent: scrollback, then a parked prompt
 *  whose row the cursor must sit on. Re-prints the prompt on SIGWINCH like a
 *  real agent redrawing after a resize. Seeded as tool `claude` with the
 *  binary overridden to the script's ABSOLUTE path; a bare `claude` would
 *  resolve through the pane shell's PATH and hit a real Claude Code install
 *  on dev boxes. */
function seedPromptbox(seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) {
  const tool = join(seedEnv.shimBin, "promptbox");
  writeFileSync(
    tool,
    `#!/bin/bash
for i in $(seq 1 30); do echo "line-$i"; done
printf '${PROMPT} '
trap "printf '\\r${PROMPT} '" WINCH
while true; do sleep 1; done
`,
  );
  chmodSync(tool, 0o755);
  const projectDir = join(seedEnv.home, "project");
  mkdirSync(projectDir, { recursive: true });
  spawnSync("git", ["init", "-q"], { cwd: projectDir });
  const addRes = spawnSync(
    resolveAoeBinary(),
    ["add", projectDir, "-t", "takeover-test", "-c", "claude", "--cmd-override", tool],
    { env: seedEnv.env },
  );
  if (addRes.status !== 0) {
    throw new Error(`aoe add failed: ${addRes.stderr?.toString() ?? "<none>"}`);
  }
}

test("ownership ping-pong keeps the cursor on the prompt row", async ({ browser }, testInfo) => {
  test.setTimeout(120_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedPromptbox,
  });
  try {
    const ctxA = await browser.newContext({ ...devices["iPhone 13"] });
    const ctxB = await browser.newContext({ ...devices["iPhone 13"], viewport: { width: 360, height: 740 } });
    const a = await ctxA.newPage();
    const b = await ctxB.newPage();

    await openLiveView(a, serve.baseUrl);
    // First client owns; no banner.
    await expect(a.locator("[data-live-takeover]")).toHaveCount(0);
    await expect.poll(() => promptAlignment(a), { timeout: 10_000 }).toEqual(ON_PROMPT);

    await openLiveView(b, serve.baseUrl);

    // B takes over; A is demoted (banner) and B aligns.
    await takeOver(b);
    await a.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
    await expect.poll(() => promptAlignment(b), { timeout: 10_000 }).toEqual(ON_PROMPT);

    // Two full take-back cycles: the reported bug was the cursor drifting
    // one row below the prompt on every take-back.
    for (let cycle = 0; cycle < 2; cycle++) {
      await takeOver(a);
      await b.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
      await expect.poll(() => promptAlignment(a), { timeout: 10_000 }).toEqual(ON_PROMPT);

      await takeOver(b);
      await a.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
      await expect.poll(() => promptAlignment(b), { timeout: 10_000 }).toEqual(ON_PROMPT);
    }

    await ctxA.close();
    await ctxB.close();
  } finally {
    await serve.stop();
  }
});

// A demoted-but-visible viewer must get ownership BACK on its own once the
// holder lets go (the other device disconnects, or the native TUI exits live
// mode and releases the lock). Before auto-reclaim, the phone stayed a
// read-only viewer with the "take over" banner until the user tapped it
// again, every single time the desktop side let go.
test("released lock auto-reclaims without another take-over tap", async ({ browser }, testInfo) => {
  test.setTimeout(120_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedPromptbox,
  });
  try {
    const ctxA = await browser.newContext({ ...devices["iPhone 13"] });
    const ctxB = await browser.newContext({ ...devices["iPhone 13"], viewport: { width: 360, height: 740 } });
    const a = await ctxA.newPage();
    const b = await ctxB.newPage();

    await openLiveView(a, serve.baseUrl);
    await expect(a.locator("[data-live-takeover]")).toHaveCount(0);

    // B steals ownership; A demotes to the banner.
    await openLiveView(b, serve.baseUrl);
    await takeOver(b);
    await a.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });

    // B disconnects, releasing the lock. A is visible at the live edge, so
    // its capture loop re-claims the vacant lock and the banner clears with
    // NO tap; the cursor re-aligns once A's grid is re-asserted.
    await ctxB.close();
    await a.locator("[data-live-takeover]").waitFor({ state: "detached", timeout: 15_000 });
    await expect.poll(() => promptAlignment(a), { timeout: 10_000 }).toEqual(ON_PROMPT);

    await ctxA.close();
  } finally {
    await serve.stop();
  }
});

// (The former "desktop click takes the size lock back" test is gone: with the
// xterm/PTY renderer removed, every client is a live client, so that scenario
// is just live-vs-live ownership handoff, covered by the ping-pong test above.
// "Desktop renders the live view" is covered by backspace-autorepeat.spec.ts.)
