#!/usr/bin/env node
// Merge Vitest and Playwright coverage into one report via
// monocart-coverage-reports. Tolerant of missing inputs so it runs cleanly
// even when one of the two suites hasn't been executed yet.
//
// Both inputs are V8-based, so they remap to the SAME web/src line map and
// merge without the double-counting that mixing istanbul-on-bundle coverage
// caused before (#2157).
//
// Inputs:
//   coverage/vitest/coverage-final.json   (v8, from `vitest run --coverage`)
//   coverage/playwright/*.v8.json         (raw Chromium V8, from the page
//                                          fixture; carries inline sourcemaps)
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
const distDir = join(webDir, "dist");
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
    console.log(`[merge-coverage] no vitest coverage at ${vitestFinal}, skipping`);
    return null;
  }
  const raw = await readFile(vitestFinal, "utf8");
  return JSON.parse(raw);
}

// The capture helper strips `source` from each V8 entry to keep the per-test
// files small (the bundle is identical every test). Re-attach it here, reading
// each served script once from `dist/` (those files carry the inline sourcemap
// monocart needs to remap back to web/src). Cached by pathname so the ~12 MB
// bundle is read once, not once per test. (#2157)
const sourceByPathname = new Map();
async function readDistSource(url) {
  let pathname;
  try {
    pathname = new URL(url).pathname;
  } catch {
    return null;
  }
  if (sourceByPathname.has(pathname)) return sourceByPathname.get(pathname);
  const file = join(distDir, pathname);
  let source = null;
  try {
    source = await readFile(file, "utf8");
  } catch {
    // Served script with no matching dist file (e.g. an injected runtime
    // script). Without source monocart can't remap it; skipping is fine.
  }
  sourceByPathname.set(pathname, source);
  return source;
}

async function loadPlaywrightCoverages() {
  if (!(await pathExists(playwrightDir))) {
    console.log(`[merge-coverage] no playwright coverage dir at ${playwrightDir}, skipping`);
    return [];
  }
  const entries = await readdir(playwrightDir);
  const jsons = entries.filter((e) => e.endsWith(".json"));
  const out = [];
  for (const e of jsons) {
    const raw = await readFile(join(playwrightDir, e), "utf8");
    let parsed;
    try {
      parsed = JSON.parse(raw);
    } catch (err) {
      console.warn(`[merge-coverage] skipping invalid JSON: ${e}: ${err}`);
      continue;
    }
    const withSource = [];
    for (const entry of parsed) {
      if (!entry.source) {
        const source = await readDistSource(entry.url);
        if (!source) continue;
        entry.source = source;
      }
      withSource.push(entry);
    }
    if (withSource.length > 0) out.push(withSource);
  }
  return out;
}

// Drop served scripts that have no business in app coverage before monocart
// spends time remapping them (vendor chunks, node_modules). Only applies to
// raw V8 entries (the playwright pass).
const entryFilter = (entry) => {
  const url = entry?.url ?? "";
  return /^https?:\/\//.test(url) && url.includes(".js") && !url.includes("node_modules");
};

// After the V8 ranges are remapped through the inline sourcemap, keep only our
// TS/JS source under web/src. Sourcemap paths arrive as `src/...`, `web/src/...`,
// or `../../src/...`, so match `src/` at the start or after a slash. Critically
// this EXCLUDES `.css` sources: the bundle's sourcemap lists imported
// stylesheets (index.css, xterm css) and monocart otherwise crashes building a
// CSS AST for them (`getCssAstInfo` on undefined). (#2157)
const sourceFilter = (sourcePath) => {
  const p = sourcePath.replace(/\\/g, "/");
  return /(^|\/)src\//.test(p) && !p.includes("node_modules") && /\.(ts|tsx|js|jsx|mjs|cjs)$/.test(p);
};

// Normalize every source to a repo-root `web/src/...` path so vitest (absolute
// paths) and playwright (bundle-relative paths) collapse onto the same key and
// merge into one file record (and match the diff paths Codecov compares against).
const sourcePath = (filePath) => filePath.replace(/^(?:.*\/)?src\//, "web/src/");

// Rewrite an istanbul coverage object's keys (and inner `.path`) to the
// normalized `web/src/...` form, dropping non-source entries. monocart only
// unions file records that share the SAME key at add() time; vitest keys are
// absolute and the playwright pass emits `web/src/...`, so without this the two
// stay separate and the later sourcePath rename collides them (one silently
// wins, dropping the other suite's hits). (#2157)
function normalizeIstanbulKeys(cov) {
  const out = {};
  for (const [key, data] of Object.entries(cov)) {
    const normalized = sourcePath(key.replace(/\\/g, "/"));
    if (!sourceFilter(normalized)) continue;
    out[normalized] = { ...data, path: normalized };
  }
  return out;
}

async function main() {
  const vitestCov = await loadVitestCoverage();
  const pwCovs = await loadPlaywrightCoverages();

  if (!vitestCov && pwCovs.length === 0) {
    console.log("[merge-coverage] no inputs found; skipping report generation");
    process.exit(0);
  }

  // monocart crashes if raw V8 (playwright) and istanbul (vitest) are added to
  // the same report, so convert the playwright V8 to istanbul-shape coverage
  // (source lines, via the inline sourcemap) in a first pass, then merge that
  // istanbul output with vitest's istanbul coverage in a second pass. Both end
  // up on the same web/src line map. (#2157)
  let pwIstanbul = null;
  if (pwCovs.length > 0) {
    const pwTmpDir = join(webDir, "coverage", ".pw-v8");
    const pwReport = new CoverageReport({
      name: "agent-of-empires playwright coverage",
      outputDir: pwTmpDir,
      entryFilter,
      sourceFilter,
      sourcePath,
      reports: [["json", { outputFile: "coverage-final.json" }]],
    });
    await pwReport.cleanCache();
    for (const cov of pwCovs) {
      await pwReport.add(cov);
    }
    await pwReport.generate();
    pwIstanbul = normalizeIstanbulKeys(JSON.parse(await readFile(join(pwTmpDir, "coverage-final.json"), "utf8")));
    console.log(
      `[merge-coverage] converted ${pwCovs.length} playwright V8 files (${Object.keys(pwIstanbul).length} sources)`,
    );
  }

  const mcr = new CoverageReport({
    name: "agent-of-empires web coverage",
    outputDir: outDir,
    sourceFilter,
    sourcePath,
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

  if (vitestCov) {
    const normalized = normalizeIstanbulKeys(vitestCov);
    await mcr.add(normalized);
    console.log(`[merge-coverage] added vitest coverage (${Object.keys(normalized).length} files)`);
  }
  if (pwIstanbul) {
    await mcr.add(pwIstanbul);
  }

  const report = await mcr.generate();
  console.log(`[merge-coverage] wrote ${outDir}`);
  console.log(`[merge-coverage] summary: lines=${report.summary?.lines?.pct ?? "?"}%`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
