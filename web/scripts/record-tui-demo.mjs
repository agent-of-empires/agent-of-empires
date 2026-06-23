// Record the TUI demo GIF (`docs/assets/demo.gif`): create a Claude Code
// session that launches into live mode, send a message, and show the session's
// status update in the sidebar while Claude works in the preview pane.
//
// The native TUI is captured by running `aoe` inside ttyd (a web terminal) and
// driving it with Playwright, then converting the recording to GIF with ffmpeg.
// (This replaced the old VHS tape: VHS renders ttyd via a headless browser
// screenshot loop that does not work on every host, while Playwright's
// recordVideo path does.)
//
// Usage:
//   node web/scripts/record-tui-demo.mjs [--port 7683] [--out path.gif]
//
// Recipe (the script does not stand up the backend):
//   1. Build:  cargo build --release --features serve   (or without --features serve)
//   2. Isolated profile with Claude creds + a git repo (see record-web-demo.mjs).
//   3. Set live mode as the attach default for the profile's config.toml:
//        [session]
//        new_session_attach_mode = "live_send"
//        default_attach_mode = "live_send"
//   4. Serve the TUI through ttyd from inside the project dir (so the New
//      Session dialog's Path defaults to it), using the DOM renderer:
//        cd $SB/home/demo-projects/my-app
//        HOME=$SB/home XDG_CONFIG_HOME=$SB/home/.config TERM=xterm-256color \
//          ttyd --port 7683 --interface 127.0.0.1 -t rendererType=dom \
//          -t fontSize=15 --writable target/release/aoe -p demo
//   5. Run this script.
// Status detection in live mode relies on the agent status hooks; accept the
// one-time prompt (this script presses `y`) or pre-enable `agent_status_hooks`.

import { chromium } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, readdirSync, statSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, cur, i, arr) => {
    if (cur.startsWith("--")) acc.push([cur.slice(2), arr[i + 1]]);
    return acc;
  }, []),
);

const port = Number(args.port ?? 7683);
const url = `http://127.0.0.1:${port}`;
const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, "..", "..");
const outGif = args.out ?? join(repoRoot, "docs", "assets", "demo.gif");

const recDir = join(repoRoot, "target", "tui-demo-recording");
rmSync(recDir, { recursive: true, force: true });
mkdirSync(recDir, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const runOrThrow = (cmd, cmdArgs) => {
  const r = spawnSync(cmd, cmdArgs, { stdio: "inherit" });
  if (r.error) throw r.error;
  if (r.status !== 0) throw new Error(`${cmd} exited with code ${r.status}`);
};

const browser = await chromium.launch({ args: ["--no-sandbox"] });
const context = await browser.newContext({
  viewport: { width: 1200, height: 720 },
  deviceScaleFactor: 1,
  recordVideo: { dir: recDir, size: { width: 1200, height: 720 } },
});
const page = await context.newPage();
const text = () => page.evaluate(() => document.body.innerText);

await page.goto(url, { waitUntil: "domcontentloaded" });
await sleep(4200);
// Skip the first-run walkthrough if it shows.
if (/Welcome to Agent of Empires/i.test(await text())) {
  await page.keyboard.press("Escape");
  await sleep(900);
}

// === Create a Claude Code session (lands in live mode) ===
await page.keyboard.press("n");
await sleep(1300);
// New Session dialog focus order: Profile -> Path -> Title. Leave Path at its
// default (the project cwd); Tab twice to Title, name it, keep tool = claude.
await page.keyboard.press("Tab");
await page.keyboard.press("Tab");
await page.keyboard.type("Claude Code", { delay: 45 });
await sleep(700);
await page.keyboard.press("Enter");
// Accept the one-time status-hooks prompt if it appears.
await sleep(1500);
if (/Agent Status Hooks/i.test(await text())) {
  await page.keyboard.press("y");
  await sleep(900);
}

// Live mode: the home list stays visible and the agent runs in the preview.
// Type as soon as Claude Code's prompt is ready so there is little dead time.
for (let i = 0; i < 20; i++) {
  await sleep(500);
  if (/for shortcuts|Welcome back/i.test(await text())) break;
}
await sleep(600);

// === Send a message via live mode and watch the sidebar status update ===
await page.keyboard.type("What does the README in this repo say? One sentence.", { delay: 40 });
await sleep(700);
await page.keyboard.press("Enter");
// Status flips to Running in the sidebar, then settles back to Idle when done.
for (let i = 0; i < 22; i++) {
  await sleep(1000);
  if (i > 6 && /Status:\s*Idle/i.test(await text())) break;
}
await sleep(2500);

await page.close();
await context.close();
await browser.close();

const webm = readdirSync(recDir)
  .filter((f) => f.endsWith(".webm"))
  .map((f) => ({ f, t: statSync(join(recDir, f)).mtimeMs }))
  .sort((a, b) => b.t - a.t)[0]?.f;
if (!webm) throw new Error("no webm produced");
const webmPath = join(recDir, webm);
console.log("recorded:", webmPath);

const palette = join(recDir, "palette.png");
const fps = 12;
const ss = "3.6"; // trim the ttyd connect + boot lead-in
const filters = `fps=${fps},scale=960:-1:flags=lanczos`;
runOrThrow("ffmpeg", ["-y", "-ss", ss, "-i", webmPath, "-vf", `${filters},palettegen=max_colors=96`, palette]);
runOrThrow("ffmpeg", [
  "-y",
  "-ss",
  ss,
  "-i",
  webmPath,
  "-i",
  palette,
  "-lavfi",
  `${filters} [x]; [x][1:v] paletteuse=dither=bayer:bayer_scale=5`,
  outGif,
]);
console.log("gif:", outGif);
process.exit(0);
