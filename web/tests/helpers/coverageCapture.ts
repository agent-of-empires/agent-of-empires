// Shared `window.__coverage__` capture for Playwright tests.
//
// Both the live config (`tests/live/*.spec.ts`) and the mocked config
// (`tests/*.spec.ts`) need to dump the istanbul coverage object after
// each test when `AOE_COVERAGE=1` is set, so `web/scripts/merge-coverage.mjs`
// can roll it up into the merged LCOV.
//
// This helper centralizes the capture so liveTest.ts and mockedTest.ts
// share one implementation.

import type { Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

export const playwrightCoverageDir = resolve(__dirname, "..", "..", "coverage", "playwright");

export async function captureCoverage(page: Page, testTitle: string): Promise<void> {
  if (process.env.AOE_COVERAGE !== "1") return;
  try {
    const coverage = await page.evaluate(() => (window as unknown as { __coverage__?: unknown }).__coverage__);
    if (!coverage) return;
    await mkdir(playwrightCoverageDir, { recursive: true });
    const safe = testTitle.replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 160);
    const filename = `${safe}-${randomUUID()}.json`;
    await writeFile(resolve(playwrightCoverageDir, filename), JSON.stringify(coverage));
  } catch {
    // Test may have closed the page or navigated cross-origin. Coverage
    // gaps in those edge cases are acceptable; we'd rather not fail the
    // test because of a coverage-collection hiccup.
  }
}
