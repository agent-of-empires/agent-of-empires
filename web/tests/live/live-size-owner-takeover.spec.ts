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

/** Vertical distance (px) between the cursor overlay and the top of the
 *  rendered row containing the prompt. 0 means perfectly aligned. */
async function cursorToPromptDelta(page: Page): Promise<number> {
  return page.evaluate((prompt) => {
    const content = document.querySelector("[data-live-content]");
    const cursor = document.querySelector("[data-live-cursor]");
    if (!content || !cursor) return Number.NaN;
    const rows = Array.from(content.children).filter((el) => !el.hasAttribute("data-live-cursor"));
    const promptRow = rows.find((el) => (el.textContent ?? "").includes(prompt));
    if (!promptRow) return Number.NaN;
    const c = cursor.getBoundingClientRect();
    const r = promptRow.getBoundingClientRect();
    return c.top - r.top;
  }, PROMPT);
}

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
    await expect.poll(async () => Math.abs(await cursorToPromptDelta(a)), { timeout: 10_000 }).toBeLessThan(2);

    await openLiveView(b, serve.baseUrl);

    // B takes over; A is demoted (banner) and B aligns.
    await takeOver(b);
    await a.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
    await expect.poll(async () => Math.abs(await cursorToPromptDelta(b)), { timeout: 10_000 }).toBeLessThan(2);

    // Two full take-back cycles: the reported bug was the cursor drifting
    // one row below the prompt on every take-back.
    for (let cycle = 0; cycle < 2; cycle++) {
      await takeOver(a);
      await b.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
      await expect.poll(async () => Math.abs(await cursorToPromptDelta(a)), { timeout: 10_000 }).toBeLessThan(2);

      await takeOver(b);
      await a.locator("[data-live-takeover]").waitFor({ state: "visible", timeout: 10_000 });
      await expect.poll(async () => Math.abs(await cursorToPromptDelta(b)), { timeout: 10_000 }).toBeLessThan(2);
    }

    await ctxA.close();
    await ctxB.close();
  } finally {
    await serve.stop();
  }
});

// (The former "desktop click takes the size lock back" test is gone: with the
// xterm/PTY renderer removed, every client is a live client, so that scenario
// is just live-vs-live ownership handoff, covered by the ping-pong test above.
// "Desktop renders the live view" is covered by backspace-autorepeat.spec.ts.)
