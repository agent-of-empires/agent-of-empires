// Playwright helpers for recording aoe dashboard demos against a live `aoe serve`.
// Env: AOE_DEMO_BASE (default http://127.0.0.1:8081), AOE_DEMO_TOKEN or
// AOE_DEMO_TOKEN_FILE (default $XDG_CONFIG_HOME/agent-of-empires-dev/serve.token),
// AOE_DEMO_RAW (directory for recorded .webm files).
import { createRequire } from "node:module";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const { chromium, devices } = createRequire(path.join(repo, "web/package.json"))("@playwright/test");
export { devices };

const tokenFile =
  process.env.AOE_DEMO_TOKEN_FILE ??
  path.join(process.env.XDG_CONFIG_HOME ?? "", "agent-of-empires-dev/serve.token");
export const TOKEN = process.env.AOE_DEMO_TOKEN ?? fs.readFileSync(tokenFile, "utf8").trim();
export const BASE = process.env.AOE_DEMO_BASE ?? "http://127.0.0.1:8081";
export const URL0 = `${BASE}/?token=${TOKEN}`;
export const RAW = process.env.AOE_DEMO_RAW ?? path.resolve("raw");

// A dot that follows mouse and touch input, since the recorder does not draw the OS cursor.
// Set window.__demoLinkTip = true to turn it blue over `cursor: pointer` and label http links with their href.
const POINTER = `(() => {
  if (window.__demoPointer) return; window.__demoPointer = true;
  const add = () => {
    const d = document.createElement('div');
    d.style.cssText = 'position:fixed;left:0;top:0;width:18px;height:18px;margin:-9px 0 0 -9px;border-radius:50%;background:rgba(255,60,60,.55);border:2px solid #fff;box-shadow:0 0 4px rgba(0,0,0,.6);pointer-events:none;z-index:2147483647;transition:transform .08s';
    const tip = document.createElement('div');
    tip.style.cssText = 'position:fixed;display:none;pointer-events:none;z-index:2147483647;background:#1d4ed8;color:#fff;font:600 12px ui-monospace,monospace;padding:3px 7px;border-radius:5px;max-width:520px;word-break:break-all';
    document.documentElement.append(d, tip);
    const at = (x, y) => { d.style.left = x + 'px'; d.style.top = y + 'px'; };
    const mv = (e) => {
      at(e.clientX, e.clientY);
      if (!window.__demoLinkTip) return;
      const t = e.target instanceof Element ? e.target : null;
      const ptr = t && getComputedStyle(t).cursor === 'pointer';
      const a = t?.closest('a[href]');
      d.style.background = ptr ? 'rgba(37,99,235,.75)' : 'rgba(255,60,60,.55)';
      d.style.borderRadius = ptr ? '4px' : '50%';
      if (a && /^https?:/.test(a.getAttribute('href'))) {
        tip.textContent = 'cursor: pointer  →  ' + a.getAttribute('href');
        tip.style.display = 'block';
        tip.style.left = Math.min(e.clientX + 14, innerWidth - 540) + 'px';
        tip.style.bottom = innerHeight - e.clientY + 14 + 'px';
      } else tip.style.display = 'none';
    };
    addEventListener('pointermove', mv, true);
    addEventListener('pointerdown', (e) => { mv(e); d.style.transform = 'scale(.6)'; }, true);
    addEventListener('pointerup', () => (d.style.transform = ''), true);
    addEventListener('touchstart', (e) => { const t = e.touches[0]; if (t) at(t.clientX, t.clientY); d.style.transform = 'scale(.6)'; }, true);
    addEventListener('touchmove', (e) => { const t = e.touches[0]; if (t) at(t.clientX, t.clientY); }, true);
    addEventListener('touchend', () => (d.style.transform = ''), true);
  };
  if (document.readyState === 'loading') addEventListener('DOMContentLoaded', add); else add();
})();`;

/** Open a recording browser. `{ mobile: true }` uses an iPhone 13 with touch. */
export async function launch(name, { mobile = false } = {}) {
  const browser = await chromium.launch();
  const dir = path.join(RAW, name);
  fs.rmSync(dir, { recursive: true, force: true });
  const context = await browser.newContext(
    mobile
      ? { ...devices["iPhone 13"], recordVideo: { dir, size: { width: 390, height: 664 } } }
      : { viewport: { width: 1280, height: 800 }, recordVideo: { dir, size: { width: 1280, height: 800 } } },
  );
  await context.addInitScript(POINTER);
  const page = await context.newPage();
  return { browser, context, page };
}

/** Close the browser and copy the clip to `$AOE_DEMO_RAW/<name>.webm`. */
export async function finish({ browser, context, page }, name) {
  const video = page.video();
  await context.close();
  await browser.close();
  const out = path.join(RAW, `${name}.webm`);
  fs.copyFileSync(await video.path(), out);
  console.log("video", out);
  return out;
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Move the mouse in visible steps. */
export const glide = (page, x, y, steps = 20) => page.mouse.move(x, y, { steps });

/** Show a caption; an empty string hides it. `{ top: true }` pins it under the top bar. */
export async function caption(page, text, { top = false } = {}) {
  await page.evaluate(
    ([t, top]) => {
      let c = document.getElementById("__demo_caption");
      if (!c) {
        c = document.createElement("div");
        c.id = "__demo_caption";
        c.style.cssText =
          "position:fixed;left:50%;transform:translateX(-50%);background:rgba(10,10,20,.85);color:#fff;font:600 15px system-ui,sans-serif;padding:8px 14px;border-radius:8px;z-index:2147483646;pointer-events:none;max-width:90vw;text-align:center";
        document.documentElement.appendChild(c);
      }
      c.style.top = top ? "64px" : "";
      c.style.bottom = top ? "" : "18px";
      c.textContent = t;
      c.style.display = t ? "block" : "none";
    },
    [text, top],
  );
}

/** Draw a highlight ring around a locator, or remove it with `on = false`. */
export async function ring(page, locator, on = true) {
  const b = on ? await locator.boundingBox() : null;
  await page.evaluate((b) => {
    let r = document.getElementById("__demo_ring");
    if (!b) return r?.remove();
    if (!r) {
      r = document.createElement("div");
      r.id = "__demo_ring";
      document.documentElement.appendChild(r);
    }
    r.style.cssText = `position:fixed;left:${b.x - 6}px;top:${b.y - 6}px;width:${b.width + 12}px;height:${b.height + 12}px;border:2px solid #ffb020;border-radius:10px;box-shadow:0 0 12px #ffb020;pointer-events:none;z-index:2147483645`;
  }, b);
}

/** Run `fn` in an unrecorded, authenticated dashboard page, e.g. to seed state through the API. */
export async function prep(fn) {
  const browser = await chromium.launch();
  const page = await (await browser.newContext()).newPage();
  await page.goto(URL0);
  await page.waitForTimeout(1500);
  const result = await page.evaluate(fn);
  await browser.close();
  return result;
}
