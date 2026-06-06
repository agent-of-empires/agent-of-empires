// Record the web dashboard demo GIFs (desktop + mobile) against a real
// `aoe serve` backend with real opencode sessions.
//
// Usage:
//   node web/scripts/record-web-demo.mjs --viewport desktop|mobile [--port 8181] [--out path.gif]
//
// The script does not stand up the backend itself. Start it once with:
//   HOME=/tmp/aoe-webdemo/home XDG_CONFIG_HOME=/tmp/aoe-webdemo/home/.config \
//     target/release/aoe serve --host 127.0.0.1 --port 8181 --no-auth
// then point this script at the same port. See assets/record-web-demo.sh
// for the full setup.

import { chromium, devices } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { mkdirSync, existsSync, readdirSync, statSync, rmSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, cur, i, arr) => {
    if (cur.startsWith("--")) acc.push([cur.slice(2), arr[i + 1]]);
    return acc;
  }, []),
);

const viewport = args.viewport ?? "desktop";
const port = Number(args.port ?? 8181);
const baseUrl = `http://127.0.0.1:${port}`;
const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, "..", "..");
const outGif =
  args.out ?? join(repoRoot, "docs", "assets", `web-${viewport}.gif`);

const recDir = join(repoRoot, "target", "web-demo-recording");
rmSync(recDir, { recursive: true, force: true });
mkdirSync(recDir, { recursive: true });

const isMobile = viewport === "mobile";
const sizeOpts = isMobile
  ? devices["iPhone 13"]
  : { viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  // Pre-start all sessions so the dashboard shows them as Idle, not Error.
  // The background status poller needs a few seconds to pick up the new state.
  const sessions = await fetch(`${baseUrl}/api/sessions`).then((r) => r.json());
  await Promise.all(
    sessions.map((s) =>
      fetch(`${baseUrl}/api/sessions/${s.id}/ensure`, { method: "POST" }),
    ),
  );
  // Wait for opencode to fully boot and the status poller to see Idle.
  // opencode takes ~8s to render its TUI; the poller runs every 2s.
  await sleep(12000);

  const browser = await chromium.launch({ args: ["--no-sandbox"] });
  const context = await browser.newContext({
    ...sizeOpts,
    recordVideo: {
      dir: recDir,
      size: isMobile
        ? { width: 390, height: 844 }
        : { width: 1280, height: 800 },
    },
  });
  const page = await context.newPage();

  await page.goto(baseUrl, { waitUntil: "networkidle" });
  await sleep(800);

  if (isMobile) {
    await runMobile(page);
  } else {
    await runDesktop(page);
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
  const filters = `fps=${fps},scale=${isMobile ? 360 : 960}:-1:flags=lanczos`;
  spawnSync(
    "ffmpeg",
    ["-y", "-i", webmPath, "-vf", `${filters},palettegen`, palette],
    { stdio: "inherit" },
  );
  spawnSync(
    "ffmpeg",
    [
      "-y",
      "-i",
      webmPath,
      "-i",
      palette,
      "-lavfi",
      `${filters} [x]; [x][1:v] paletteuse=dither=bayer:bayer_scale=5`,
      outGif,
    ],
    { stdio: "inherit" },
  );
  console.log("gif:", outGif);
  process.exit(0);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});

async function runDesktop(page) {
  // Land on dashboard with sidebar visible. Two pre-created opencode
  // sessions are listed under their project groups.
  await sleep(1500);

  // Open the first session. The session list buttons are nested inside
  // the sidebar's project groups; filter by visible label and grab the
  // first match (the project header for "api-server" is lowercase, so
  // "API Server" only matches the session row).
  await sidebarSession(page, "API Server").click();
  await page.waitForSelector(".xterm", { timeout: 15_000 });
  // opencode TUI takes a beat to draw its first frame.
  await sleep(5000);

  await page.locator(".xterm-helper-textarea").first().focus();
  await page.keyboard.type("write a haiku about parallel agents", {
    delay: 40,
  });
  await sleep(400);
  await page.keyboard.press("Enter");
  // Wait for the response to stream in.
  await sleep(9000);

  // Switch to the second session to show parallelism.
  await sidebarSession(page, "Web App").click();
  await sleep(2500);
  await page.locator(".xterm-helper-textarea").first().focus();
  await page.keyboard.type("list 3 ways to use git worktrees", {
    delay: 40,
  });
  await sleep(400);
  await page.keyboard.press("Enter");
  await sleep(8000);

  // Brief help overlay so viewers see keyboard shortcuts exist.
  await page.evaluate(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "?", bubbles: true }),
    );
  });
  await sleep(2500);
  await page.keyboard.press("Escape");
  await sleep(800);
}

function sidebarSession(page, label) {
  // Session row buttons render as "<status-glyph> <label>" — substring match
  // against the visible label is enough; project group rows use the kebab
  // form ("api-server" vs "API Server"), so they don't collide.
  return page.getByRole("button").filter({ hasText: label }).first();
}

async function runMobile(page) {
  await sleep(1200);
  // Hamburger → sidebar → tap session.
  await page.getByRole("button", { name: "Toggle sidebar" }).click();
  await sleep(900);
  await sidebarSession(page, "API Server").click();
  await page.waitForSelector(".xterm", { timeout: 15_000 });
  await sleep(5500);

  // Mobile types via the floating keyboard FAB; tap it then use the soft
  // keyboard area. Falls back to focusing xterm-helper-textarea if the
  // FAB selector ever changes.
  await page.locator(".xterm-helper-textarea").first().focus();
  await page.keyboard.type("haiku about phones and agents", { delay: 50 });
  await sleep(500);
  await page.keyboard.press("Enter");
  await sleep(8000);

  // Show that scrolling works on mobile.
  await page
    .locator(".xterm-viewport")
    .first()
    .evaluate((el) => {
      el.scrollBy({ top: 200, behavior: "smooth" });
    });
  await sleep(1500);
}
