// Record the web dashboard demo GIFs (desktop + mobile) against a real
// `aoe serve` backend driving a real Claude Code session in the structured
// (ACP) view. Each recording: open the dashboard, create a Claude Code
// session through the wizard, send a message, and show the session's status
// update in the sidebar.
//
// Usage:
//   node web/scripts/record-web-demo.mjs --viewport desktop|mobile \
//     --project /abs/path/to/repo [--port 8182] [--out path.gif]
//
// Recipe (the script does not stand up the backend):
//   1. Build with the web dashboard:           cargo build --release --features serve
//   2. Isolated profile with Claude creds:
//        SB=/tmp/aoe-webdemo
//        mkdir -p "$SB/home/.claude"
//        cp ~/.claude/.credentials.json "$SB/home/.claude/.credentials.json"
//        printf '{"hasCompletedOnboarding":true,"theme":"dark"}' > "$SB/home/.claude.json"
//        git init "$SB/home/demo-projects/my-app" && (cd "$_" && git commit --allow-empty -m init)
//   3. Open the project once so it lands in the wizard's "Recent" list
//      (e.g. create + delete a session), then start the server:
//        HOME=$SB/home XDG_CONFIG_HOME=$SB/home/.config \
//          target/release/aoe serve --host 127.0.0.1 --port 8182 --no-auth -p webdemo
//   4. Run this script per viewport, pointing --project at the repo from step 2.
// The Claude structured view needs the `claude-agent-acp` adapter (>=0.49):
//   npm install -g @agentclientprotocol/claude-agent-acp@latest
// Between runs, delete the session (the wizard creates a worktree branch named
// after the title) and prune the branch so re-creates don't collide.

import { chromium, devices } from "@playwright/test";
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

const viewport = args.viewport ?? "desktop";
const isMobile = viewport === "mobile";
const port = Number(args.port ?? 8182);
const baseUrl = `http://127.0.0.1:${port}`;
const project = args.project;
if (!project) throw new Error("pass --project /abs/path/to/repo (must be in the wizard's Recent list)");
const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, "..", "..");
const outGif = args.out ?? join(repoRoot, "docs", "assets", `web-${viewport}.gif`);

const recDir = join(repoRoot, "target", "web-demo-recording");
rmSync(recDir, { recursive: true, force: true });
mkdirSync(recDir, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const listSessions = async () => {
  try {
    const a = await (await fetch(`${baseUrl}/api/sessions`)).json();
    return a.sessions ?? [];
  } catch {
    return [];
  }
};
const statusOf = async (id) => (await listSessions()).find((s) => s.id === id)?.status ?? "(none)";
const runOrThrow = (cmd, cmdArgs) => {
  const r = spawnSync(cmd, cmdArgs, { stdio: "inherit" });
  if (r.error) throw r.error;
  if (r.status !== 0) throw new Error(`${cmd} exited with code ${r.status}`);
};

const sizeOpts = isMobile ? devices["iPhone 13"] : { viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 };

const browser = await chromium.launch({ args: ["--no-sandbox"] });
const context = await browser.newContext({
  ...sizeOpts,
  // Match the iPhone 13 viewport height so there is no letterbox padding.
  recordVideo: { dir: recDir, size: isMobile ? { width: 390, height: 664 } : { width: 1280, height: 800 } },
});
const page = await context.newPage();

await page.goto(baseUrl, { waitUntil: "networkidle" });
await sleep(500);
const notNow = page.getByRole("button", { name: "Not now" });
if (await notNow.count())
  await notNow
    .first()
    .click()
    .catch(() => {});
// Hold on the dashboard a moment so the starting point reads before the wizard.
await sleep(1500);

// === Create a Claude Code session via the wizard ===
await page
  .getByRole("button", { name: /Pick a project/i })
  .first()
  .click();
await sleep(900);
await page.getByText(project, { exact: true }).first().click(); // pick it from "Recent"
await sleep(700);
const title = page.getByPlaceholder(/Auto-generated/i);
if (await title.count()) await title.first().fill("Claude Code");
await sleep(500);
await page.getByRole("button", { name: "claude", exact: true }).first().click();
await sleep(700);
const idsBefore = new Set((await listSessions()).map((s) => s.id));
await page
  .getByRole("button", { name: /Launch session/i })
  .first()
  .click();

// Wait for the new session to appear and its structured view to be ready
// (agent Idle + composer visible). Track it by id so a stray pre-existing
// session never confuses the status waits.
const composer = page.getByPlaceholder(/Send a message/i).first();
let sessionId = null;
for (let i = 0; i < 40; i++) {
  await sleep(1000);
  const fresh = (await listSessions()).find((s) => !idsBefore.has(s.id));
  if (fresh) sessionId = fresh.id;
  if (
    sessionId &&
    (await statusOf(sessionId)) === "Idle" &&
    (await composer.count()) &&
    (await composer.isVisible().catch(() => false))
  )
    break;
}
await sleep(1500);

// === Send a message and watch the status update in the sidebar ===
await composer.click();
await composer.fill("In one sentence, what does the README in this repo say?");
await sleep(700);
await page.getByRole("button", { name: "Send message" }).first().click();
await sleep(1800); // status flips to Running

if (isMobile) {
  // Reveal the sidebar so the session's Running status is visible.
  const toggle = page.getByRole("button", { name: "Toggle sidebar" });
  if (await toggle.count()) {
    await toggle
      .first()
      .click()
      .catch(() => {});
    await sleep(2800);
  }
  for (let i = 0; i < 24; i++) {
    await sleep(800);
    if ((await statusOf(sessionId)) === "Idle" && i > 2) break;
  }
  await sleep(2500);
} else {
  // Desktop keeps the sidebar visible; just let the answer stream in.
  for (let i = 0; i < 24; i++) {
    await sleep(800);
    if ((await statusOf(sessionId)) === "Idle" && i > 4) break;
  }
  await sleep(2500);
}

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
const ss = "0.7"; // trim the initial page-load lead-in (keep a little dashboard dwell)
const filters = `fps=${fps},scale=${isMobile ? 360 : 960}:-1:flags=lanczos`;
runOrThrow("ffmpeg", ["-y", "-ss", ss, "-i", webmPath, "-vf", `${filters},palettegen=max_colors=128`, palette]);
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
