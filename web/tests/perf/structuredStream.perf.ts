// Structured view streaming benchmark (issue #5792). Not part of the normal test runs.
//
// Run: `cargo build --features web` from the repo root, then
//   cd web && PERF_RUNS=5 PERF_OUT=/tmp/perf.jsonl npx playwright test --config playwright.perf.config.ts
// The aoe binary comes from resolveAoeBinary(): $AOE_E2E_BINARY, else target/debug/aoe, else
// target/release/aoe. Each run prints one JSON line; PERF_OUT appends the same lines to a file.
// PERF_DIFF_LINES sets the Edit diff size (default 200, see structuredScript.ts).

import { appendFileSync } from "node:fs";
import type { Page } from "@playwright/test";
import { test, expect } from "../helpers/liveTest";
import { composer, openStructuredView, postPrompt, releaseTurn, startAcpSession, stopButton } from "../helpers/acp";
import {
  HISTORY_DONE,
  HISTORY_FENCE_SEED,
  MEASURED_CHUNKS,
  MEASURED_DONE,
  MEASURED_FENCE_SEED,
  structuredScript,
} from "./structuredScript";

const RUNS = Number(process.env.PERF_RUNS) || 5;
const TYPED_BURST = "x".repeat(10);
const MAX_TYPING_BURSTS = 400;

interface RunResult {
  scriptLayoutMsPerChunk: number;
  commitsPerChunk: number;
  longTasks: number;
  maxTypingLatencyMs: number;
}

const results: RunResult[] = [];

/** Median and worst (max) per metric. */
function summarize(runs: RunResult[]) {
  const median = (xs: number[]) => {
    const s = [...xs].sort((a, b) => a - b);
    const mid = s.length >> 1;
    return s.length % 2 ? s[mid]! : (s[mid - 1]! + s[mid]!) / 2;
  };
  const out: Record<string, { median: number; worst: number }> = {};
  for (const key of Object.keys(runs[0] ?? {}) as (keyof RunResult)[]) {
    const xs = runs.map((r) => r[key]);
    out[key] = { median: median(xs), worst: Math.max(...xs) };
  }
  return out;
}

/** Poll the tail of the replay log; `replay?since=0` is paged at 1,000 frames and never reaches the end. */
async function waitForReplayTail(baseUrl: string, sessionId: string, needle: string): Promise<void> {
  const replay = async (since: number) =>
    (await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=${since}`)).json();
  await expect
    .poll(
      async () => {
        const { highest_seq } = await replay(Number.MAX_SAFE_INTEGER);
        return JSON.stringify(await replay(Math.max(0, highest_seq - 50))).includes(needle);
      },
      { timeout: 120_000, intervals: [1_000] },
    )
    .toBe(true);
}

/** Wait until the DOM node count holds steady, so late history rendering cannot land in the measured turn. */
async function waitForQuiet(page: Page): Promise<void> {
  let last = -1;
  let steady = 0;
  for (let i = 0; i < 60 && steady < 6; i++) {
    await page.waitForTimeout(2_000);
    const nodes = await page.evaluate(() => document.getElementsByTagName("*").length);
    steady = nodes === last ? steady + 1 : 0;
    last = nodes;
  }
  if (steady < 6) throw new Error("history render never settled before the measured turn");
}

async function installProbes(page: Page) {
  await page.addInitScript(() => {
    const w = window as unknown as Record<string, unknown>;
    w.__reactCommits = 0;
    w.__longTasks = [] as number[];
    w.__eventDurations = [] as number[];
    w.__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
      supportsFiber: true,
      renderers: new Map(),
      inject: () => 1,
      onCommitFiberRoot: () => {
        (w.__reactCommits as number)++;
      },
      onCommitFiberUnmount: () => {},
      onScheduleFiberRoot: () => {},
    };
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) (w.__longTasks as number[]).push(e.duration);
    }).observe({ type: "longtask", buffered: true });
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) (w.__eventDurations as number[]).push(e.duration);
    }).observe({ type: "event", durationThreshold: 16, buffered: true } as PerformanceObserverInit);
  });
}

async function cdpMetrics(cdp: Awaited<ReturnType<ReturnType<Page["context"]>["newCDPSession"]>>) {
  const { metrics } = await cdp.send("Performance.getMetrics");
  const get = (name: string) => metrics.find((m) => m.name === name)?.value ?? 0;
  return { busyMs: (get("ScriptDuration") + get("LayoutDuration")) * 1000 };
}

for (let run = 1; run <= RUNS; run++) {
  test(`structured view streaming run ${run}/${RUNS}`, async ({ page, spawnServe }) => {
    await installProbes(page);
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: "perf-stream",
      fakeAcpScript: structuredScript(),
    });
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Performance.enable");

    // The first prompt consumes the history turn. Stream it with no page open, then load it from the replay.
    await postPrompt(serve.baseUrl, sessionId, "build history");
    await waitForReplayTail(serve.baseUrl, sessionId, HISTORY_DONE);
    await openStructuredView(page, serve, sessionId);
    await expect(page.getByText(HISTORY_DONE)).toBeAttached({ timeout: 60_000 });
    await waitForQuiet(page);
    const codeBlocks = (seed: string) => page.locator(".acp-code-block", { hasText: `const ${seed} =` });
    await expect(codeBlocks(HISTORY_FENCE_SEED)).toHaveCount(10);

    // The second prompt starts the measured turn, which parks on the release gate.
    const box = composer(page);
    await box.fill("measured turn");
    await box.press("Enter");
    await expect(stopButton(page)).toBeVisible({ timeout: 15_000 });
    await page.waitForTimeout(1_000);

    await page.evaluate(() => {
      const w = window as unknown as Record<string, unknown>;
      w.__reactCommits = 0;
      w.__longTasks = [];
      w.__eventDurations = [];
    });
    const before = await cdpMetrics(cdp);
    releaseTurn(serve);

    // Type for the whole turn, clearing between bursts so the composer stays small.
    await box.focus();
    let stopTyping = false;
    let typingError: unknown;
    const typing = (async () => {
      for (let i = 0; i < MAX_TYPING_BURSTS && !stopTyping; i++) {
        await page.keyboard.type(TYPED_BURST, { delay: 50 });
        await box.fill("");
      }
    })().catch((e) => {
      typingError = e;
    });
    try {
      await expect(page.getByText(MEASURED_DONE)).toBeAttached({ timeout: 120_000 });
      await expect(stopButton(page)).toBeHidden({ timeout: 30_000 });
      await expect(codeBlocks(MEASURED_FENCE_SEED)).toHaveCount(1);
    } finally {
      stopTyping = true;
      await typing;
    }
    if (typingError) throw typingError;
    const after = await cdpMetrics(cdp);

    const probe = await page.evaluate(() => {
      const w = window as unknown as Record<string, number[] | number>;
      return {
        commits: w.__reactCommits as number,
        longTasks: (w.__longTasks as number[]).filter((d) => d > 50).length,
        maxEvent: Math.max(0, ...(w.__eventDurations as number[])),
        domNodes: document.getElementsByTagName("*").length,
      };
    });
    const result: RunResult = {
      scriptLayoutMsPerChunk: (after.busyMs - before.busyMs) / MEASURED_CHUNKS,
      commitsPerChunk: probe.commits / MEASURED_CHUNKS,
      longTasks: probe.longTasks,
      maxTypingLatencyMs: probe.maxEvent,
    };
    results.push(result);
    const line = JSON.stringify({ run, ...result, domNodes: probe.domNodes });
    console.log(line);
    if (process.env.PERF_OUT) appendFileSync(process.env.PERF_OUT, `${line}\n`);
  });
}

test.afterAll(() => {
  if (results.length) console.log(`perf summary (${results.length} runs): ${JSON.stringify(summarize(results))}`);
});
