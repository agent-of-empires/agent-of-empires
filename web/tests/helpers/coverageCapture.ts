// Chromium V8 coverage capture for Playwright tests.
//
// Both the live config (`tests/live/*.spec.ts`) and the mocked config
// (`tests/*.spec.ts`) collect raw V8 JS coverage when `AOE_COVERAGE=1` is set,
// so `web/scripts/merge-coverage.mjs` (monocart) can remap it back to web/src
// through the bundle's inline sourcemap and merge it with vitest's v8 coverage
// on the SAME source line map. (Previously these dumped istanbul
// `window.__coverage__` from a vite-plugin-istanbul build, which numbered the
// bundle differently than vitest and made Codecov drop vitest's hits. #2157)
//
// V8 coverage is Chromium-only; `startCoverage` returns false (and the spec
// runs uninstrumented) on other engines or if the API is unavailable.

import type { Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

export const playwrightCoverageDir = resolve(__dirname, "..", "..", "coverage", "playwright");

// Must run before the spec's first navigation (specs navigate in their body),
// hence `resetOnNavigation: false` so SPA route changes and reloads don't wipe
// earlier coverage. Returns whether collection actually started, so teardown
// knows whether there's anything to stop.
export async function startCoverage(page: Page): Promise<boolean> {
  if (process.env.AOE_COVERAGE !== "1") return false;
  try {
    await page.coverage.startJSCoverage({ resetOnNavigation: false, reportAnonymousScripts: false });
    return true;
  } catch {
    // Non-Chromium engine or the page/context is already gone. Coverage gaps
    // here are acceptable; never fail a test over coverage collection.
    return false;
  }
}

export async function stopAndWriteCoverage(page: Page, testTitle: string, started: boolean): Promise<void> {
  if (process.env.AOE_COVERAGE !== "1" || !started) return;
  try {
    const coverage = await page.coverage.stopJSCoverage();
    // Keep only real served app scripts. node_modules and non-http entries
    // (extensions, injected eval) are noise; final web/src scoping happens in
    // the merge script's sourceFilter after sourcemap remapping.
    //
    // Drop `source`: it's the full bundle (~12 MB, identical every test) and
    // would balloon the suite's coverage to gigabytes. The merge script reads
    // each script's source (carrying its inline sourcemap) once from `dist/`.
    const entries = coverage
      .filter((e) => {
        const url = e.url ?? "";
        return /^https?:\/\//.test(url) && url.includes(".js") && !url.includes("node_modules");
      })
      .map((e) => ({ url: e.url, functions: e.functions }));
    if (entries.length === 0) return;
    await mkdir(playwrightCoverageDir, { recursive: true });
    const safe = testTitle.replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 160);
    const filename = `${safe}-${randomUUID()}.v8.json`;
    await writeFile(resolve(playwrightCoverageDir, filename), JSON.stringify(entries));
  } catch {
    // Test may have closed the page or navigated cross-origin. Coverage gaps
    // in those edge cases are acceptable; we'd rather not fail the test.
  }
}
