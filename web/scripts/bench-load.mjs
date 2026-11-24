// Cold-load + first-diff benchmark for the dashboard.
// Usage: node scripts/bench-load.mjs <aoe-binary> [runs=5]
// Starts an isolated `aoe serve`, throttles Chrome to 150 ms RTT / 10 Mbit/s with the
// cache off, and prints medians: cold-load bytes, script CPU, and first-diff cost.
import { chromium } from "@playwright/test";
import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:net";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

// The live fixture's isolation policy, loaded the way check-env-isolation.mjs loads it.
const helper = join(dirname(fileURLToPath(import.meta.url)), "../tests/helpers/isolatedEnv.ts");
const { isolateEnv } = await import(
  "data:text/javascript;base64," +
    Buffer.from(
      ts.transpileModule(readFileSync(helper, "utf8"), {
        compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
      }).outputText,
    ).toString("base64")
);

// Resolve relative paths now; child processes run with other working directories.
const arg = process.argv[2];
const bin = arg?.includes("/") ? resolve(arg) : arg;
const runs = Number(process.argv[3] ?? 5);
if (!arg) throw new Error("usage: bench-load.mjs <aoe-binary> [runs]");

const home = mkdtempSync(join(tmpdir(), "aoe-bench-home-"));
const repo = mkdtempSync(join(tmpdir(), "aoe-bench-repo-"));
// The OS hands out a free port; the daemon must bind it or exit (checked below).
const port = await new Promise((ok) => {
  const probe = createServer().listen(0, "127.0.0.1", () => {
    const { port: p } = probe.address();
    probe.close(() => ok(p));
  });
});
const tmuxSocket = join(home, "tmux.sock");
const tmp = join(home, "tmp");
const tmuxTmp = join(home, "tmux-tmp");
for (const d of [tmp, tmuxTmp]) mkdirSync(d);
const env = {
  ...isolateEnv(process.env, {
    home,
    xdgConfig: join(home, ".config"),
    xdgData: join(home, ".local/share"),
    tmp,
    tmuxTmp,
  }),
  XDG_STATE_HOME: join(home, ".local/state"),
  AOE_TMUX_SOCKET: tmuxSocket,
};
const sh = (cmd, args, cwd) => execFileSync(cmd, args, { cwd, env, stdio: "pipe" });

sh("git", ["init", "-q", "-b", "main"], repo);
sh("git", ["config", "user.email", "b@b"], repo);
sh("git", ["config", "user.name", "bench"], repo);
writeFileSync(join(repo, "example.ts"), "export const x = 1;\n");
sh("git", ["add", "."], repo);
sh("git", ["commit", "-qm", "init"], repo);
writeFileSync(
  join(repo, "example.ts"),
  'import { useState } from "react";\nexport function greet(name: string): string {\n  return `hi ${name}`;\n}\n',
);

const median = (a) => {
  const v = [...a].sort((x, y) => x - y);
  const mid = v.length >> 1;
  return v.length % 2 ? v[mid] : (v[mid - 1] + v[mid]) / 2;
};
// Script CPU (ms) from a Chrome trace: main-thread compile + module evaluate, and
// off-thread streaming parse of every script (workers included).
const scriptMs = (trace) => {
  const events = trace.traceEvents ?? trace;
  const threads = new Map();
  for (const e of events) if (e.name === "thread_name") threads.set(`${e.pid}:${e.tid}`, e.args.name);
  const out = { mainCompile: 0, mainEvaluate: 0, streamParse: 0 };
  for (const e of events) {
    if (e.ph !== "X" || !e.dur) continue;
    const t = threads.get(`${e.pid}:${e.tid}`) ?? "";
    const ms = e.dur / 1000;
    if (t === "CrRendererMain") {
      if (e.name === "V8.CompileCode") out.mainCompile += ms;
      if (e.name === "v8.evaluateModule" || e.name === "EvaluateScript") out.mainEvaluate += ms;
    } else if (e.name === "BackgroundJSStreamManager::RunScriptStreamingTask") out.streamParse += ms;
  }
  return out;
};
const highlighted = () => {
  const colors = new Set();
  const walk = (root) =>
    root.querySelectorAll("*").forEach((el) => {
      const c = el.style?.color;
      if (c && c.startsWith("rgb")) colors.add(c);
      if (el.shadowRoot) walk(el.shadowRoot);
    });
  document.querySelectorAll("diffs-container").forEach((h) => h.shadowRoot && walk(h.shadowRoot));
  return colors.size >= 3;
};

const R = {
  coldBytes: [],
  entryBytes: [],
  coldMainCompile: [],
  coldMainEvaluate: [],
  diffBytes: [],
  diffWorkerBytes: [],
  diffStreamParse: [],
  diffMainEvaluate: [],
  timeToHighlight: [],
  workerFiles: [],
};
const cats = ["devtools.timeline", "v8", "disabled-by-default-v8.compile"];
let server;
let browser;
try {
  server = spawn(
    bin,
    ["serve", "--no-auth", "--behind-proxy", "--allowed-host", "bench.local", "--no-autostart", "--port", String(port)],
    { env, stdio: "ignore" },
  );
  const url = `http://127.0.0.1:${port}/`;
  // The daemon writes serve.url under the isolated home only after it binds the port,
  // so a listener we did not spawn cannot satisfy this wait.
  const published = () => {
    const f = readdirSync(home, { recursive: true }).find((p) => p.endsWith("serve.url"));
    return f !== undefined && readFileSync(join(home, f), "utf8").includes(`:${port}/`);
  };
  let up = false;
  for (let i = 0; i < 600 && !up; i++) {
    if (server.exitCode !== null) throw new Error(`aoe serve exited with ${server.exitCode}`);
    if (published()) {
      try {
        up = (await fetch(url)).ok;
      } catch {}
    }
    if (!up) await new Promise((r) => setTimeout(r, 250));
  }
  if (!up || server.exitCode !== null) throw new Error("aoe serve did not come up");

  // The CLI attaches to the serve daemon; the command must name a supported tool to keep a pane alive.
  execFileSync(bin, ["add", repo, "-t", "bench", "-c", "sh -c 'echo claude; exec sh'"], {
    cwd: repo,
    env: { ...env, AOE_DAEMON_URL: url.slice(0, -1) },
    stdio: "pipe",
  });

  browser = await chromium.launch();
  for (let i = 0; i < runs; i++) {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await ctx.newPage();
    const cdp = await ctx.newCDPSession(page);
    await cdp.send("Network.enable");
    await cdp.send("Network.setCacheDisabled", { cacheDisabled: true });
    await cdp.send("Network.emulateNetworkConditions", {
      offline: false,
      latency: 150,
      downloadThroughput: (10 * 1024 * 1024) / 8,
      uploadThroughput: (10 * 1024 * 1024) / 8,
    });
    const done = [];
    const live = new Set();
    page.on("worker", (w) => {
      live.add(w);
      w.on("close", () => live.delete(w));
    });
    // content-length is the encoded body size; worker requests expose nothing richer.
    ctx.on("response", (res) => done.push([res.url(), Number(res.headers()["content-length"] ?? 0)]));
    const total = (f = () => true) => done.filter(([u]) => f(u)).reduce((n, [, b]) => n + b, 0);

    await browser.startTracing(page, { categories: cats });
    await page.goto(url, { waitUntil: "load" });
    await page.getByTestId("sidebar-session-row").first().waitFor({ timeout: 60000 });
    const cold = scriptMs(JSON.parse((await browser.stopTracing()).toString()));
    await page.waitForTimeout(500);
    R.coldBytes.push(total());
    R.entryBytes.push(total((u) => /\/assets\/index-.*\.js/.test(u)));
    R.coldMainCompile.push(cold.mainCompile);
    R.coldMainEvaluate.push(cold.mainEvaluate);
    const before = total();
    const beforeWorker = total((u) => /\/assets\/worker-/.test(u));

    await browser.startTracing(page, { categories: cats });
    await page.getByTestId("sidebar-session-row").first().click();
    await page.getByText("example.ts").first().waitFor({ timeout: 30000 });
    const t0 = Date.now();
    await page.getByText("example.ts").first().click();
    await page.waitForFunction(highlighted, null, { timeout: 120000 });
    const elapsed = Date.now() - t0;
    // Main-thread fallback also highlights. The pool terminates its workers when it falls back
    // (including on init timeout), so require a worker still alive and no fallback banner.
    if (
      ![...live].some((w) => /\/assets\/worker-/.test(w.url())) ||
      (await page.getByText("Highlighting on the main thread").count())
    ) {
      throw new Error(
        `diff was not highlighted by a live worker (live: ${JSON.stringify([...live].map((w) => w.url()))})`,
      );
    }
    R.timeToHighlight.push(elapsed);
    await page.waitForTimeout(2000);
    const diff = scriptMs(JSON.parse((await browser.stopTracing()).toString()));
    R.diffBytes.push(total() - before);
    R.diffWorkerBytes.push(
      total((u) => /\/assets\/(worker|shiki|langs|core|engine|stream-tokenizer)-/.test(u)) - beforeWorker,
    );
    R.diffStreamParse.push(diff.streamParse);
    R.diffMainEvaluate.push(diff.mainEvaluate);
    R.workerFiles = done.filter(([u]) => /worker-/.test(u)).map(([u, b]) => [u.split("/").pop(), b]);
    await ctx.close();
  }
} finally {
  await browser?.close();
  server?.kill();
  try {
    sh(bin, ["serve", "--stop"], repo);
  } catch {}
  try {
    execFileSync("tmux", ["-S", tmuxSocket, "kill-server"], { stdio: "ignore" });
  } catch {}
}
const m = Object.fromEntries(
  Object.entries(R)
    .filter(([k]) => k !== "workerFiles")
    .map(([k, v]) => [k, Math.round(median(v))]),
);
console.log(JSON.stringify({ runs, median: m, workerFiles: R.workerFiles, raw: R }, null, 1));
