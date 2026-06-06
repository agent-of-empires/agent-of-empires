#!/usr/bin/env node
// Merge Vitest (v8) and Playwright (istanbul) coverage into one report via
// monocart-coverage-reports. Tolerant of missing inputs so it runs cleanly
// even when one of the two suites hasn't been executed yet.
//
// Inputs:
//   coverage/vitest/coverage-final.json   (v8, from `vitest run --coverage`)
//   coverage/playwright/*.json            (istanbul, from afterEach hook)
//
// Output:
//   coverage/merged/lcov.info
//   coverage/merged/coverage-summary.json
//   coverage/merged/index.html
//
// Usage:
//   node web/scripts/merge-coverage.mjs

import { readdir, readFile, stat } from "node:fs/promises";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { existsSync } from "node:fs";
import { CoverageReport } from "monocart-coverage-reports";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const webDir = resolve(__dirname, "..");
const vitestFinal = join(webDir, "coverage", "vitest", "coverage-final.json");
const playwrightDir = join(webDir, "coverage", "playwright");
const outDir = join(webDir, "coverage", "merged");

async function pathExists(p) {
  try {
    await stat(p);
    return true;
  } catch {
    return false;
  }
}

async function loadVitestCoverage() {
  if (!(await pathExists(vitestFinal))) {
    console.log(
      `[merge-coverage] no vitest coverage at ${vitestFinal}, skipping`,
    );
    return null;
  }
  const raw = await readFile(vitestFinal, "utf8");
  return JSON.parse(raw);
}

async function loadPlaywrightCoverages() {
  if (!(await pathExists(playwrightDir))) {
    console.log(
      `[merge-coverage] no playwright coverage dir at ${playwrightDir}, skipping`,
    );
    return [];
  }
  const entries = await readdir(playwrightDir);
  const jsons = entries.filter((e) => e.endsWith(".json"));
  const out = [];
  for (const e of jsons) {
    const raw = await readFile(join(playwrightDir, e), "utf8");
    try {
      out.push(JSON.parse(raw));
    } catch (err) {
      console.warn(`[merge-coverage] skipping invalid JSON: ${e}: ${err}`);
    }
  }
  return out;
}

async function main() {
  const mcr = new CoverageReport({
    name: "agent-of-empires web coverage",
    outputDir: outDir,
    sourcePath: (filePath) => filePath.replace(/^.*\/src\//, "web/src/"),
    reports: [
      ["v8"],
      ["lcov"],
      ["html"],
      ["console-summary"],
      // `coverage-summary.json` + `coverage-final.json` are read by
      // `davelosert/vitest-coverage-report-action` in CI (see ci.yml
      // `coverage` job) to post the per-PR comment. The action accepts
      // any istanbul-shaped summary, not only vitest's, so the merged
      // report (vitest + playwright) flows through it cleanly.
      ["json-summary", { outputFile: "coverage-summary.json" }],
      ["json", { outputFile: "coverage-final.json" }],
    ],
  });

  await mcr.cleanCache();

  const vitestCov = await loadVitestCoverage();
  if (vitestCov) {
    await mcr.add(vitestCov);
    console.log(
      `[merge-coverage] added vitest coverage (${Object.keys(vitestCov).length} files)`,
    );
  }

  const pwCovs = await loadPlaywrightCoverages();
  for (const cov of pwCovs) {
    await mcr.add(cov);
  }
  if (pwCovs.length > 0) {
    console.log(
      `[merge-coverage] added ${pwCovs.length} playwright coverage files`,
    );
  }

  if (!vitestCov && pwCovs.length === 0) {
    console.log("[merge-coverage] no inputs found; skipping report generation");
    process.exit(0);
  }

  const report = await mcr.generate();
  console.log(`[merge-coverage] wrote ${outDir}`);
  console.log(
    `[merge-coverage] summary: lines=${report.summary?.lines?.pct ?? "?"}%`,
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
