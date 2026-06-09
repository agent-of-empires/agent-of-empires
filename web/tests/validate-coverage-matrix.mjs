#!/usr/bin/env node
// Validate web/tests/coverage-matrix.json.
//
// Checks:
//   1. Every `specs[]` path referenced exists on disk (relative to web/).
//   2. Every `deferred` entry has an `issue` URL.
//   3. Every `out-of-scope` entry has a `reason`.
//   4. Every `.tsx` file under web/src/components/** (excluding __tests__,
//      .test.tsx, and .stories.tsx) appears in either:
//        a. some matrix entry's `components[]`, or
//        b. coverage-matrix.exempt.json's `exempt[].path`.
//   5. Every exempt entry has a `reason`.
//
// Run from anywhere; paths are resolved relative to this file.
//
// Exits 0 on success, prints a list of failures and exits 1 on failure.

import { readdir, readFile, stat } from "node:fs/promises";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const webDir = resolve(__dirname, "..");
const repoRoot = resolve(webDir, "..");

const matrixPath = resolve(__dirname, "coverage-matrix.json");
const exemptPath = resolve(__dirname, "coverage-matrix.exempt.json");
const componentsRoot = resolve(webDir, "src", "components");

async function pathExists(p) {
  try {
    await stat(p);
    return true;
  } catch {
    return false;
  }
}

async function walkTsx(dir) {
  const out = [];
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "__tests__") continue;
      out.push(...(await walkTsx(full)));
    } else if (entry.isFile()) {
      if (!entry.name.endsWith(".tsx")) continue;
      if (entry.name.endsWith(".test.tsx")) continue;
      if (entry.name.endsWith(".stories.tsx")) continue;
      out.push(full);
    }
  }
  return out;
}

function normalize(p) {
  // Anything to "web/src/..." regardless of cwd.
  const rel = p.startsWith(repoRoot + "/") ? p.slice(repoRoot.length + 1) : p;
  return rel.replace(/\\/g, "/");
}

async function main() {
  const errors = [];

  // Load matrix + exempt.
  const matrix = JSON.parse(await readFile(matrixPath, "utf8"));
  const exempt = JSON.parse(await readFile(exemptPath, "utf8"));
  const exemptPaths = new Set(exempt.exempt.map((e) => e.path));

  for (const e of exempt.exempt) {
    if (!e.reason || typeof e.reason !== "string") {
      errors.push(`coverage-matrix.exempt.json: entry '${e.path}' is missing 'reason'`);
    }
  }

  // Check matrix entry shape.
  const seenComponents = new Set();
  for (const surface of matrix.surfaces ?? []) {
    if (!surface.id) {
      errors.push("coverage-matrix.json: surface is missing 'id'");
      continue;
    }
    const tag = surface.id;
    if (surface.kind === "deferred") {
      if (!surface.issue) {
        errors.push(`coverage-matrix.json: deferred surface '${tag}' is missing 'issue' link`);
      }
    } else if (surface.kind === "out-of-scope") {
      if (!surface.reason) {
        errors.push(`coverage-matrix.json: out-of-scope surface '${tag}' is missing 'reason'`);
      }
    } else if (
      surface.kind === "live-playwright" ||
      surface.kind === "vitest" ||
      surface.kind === "mocked-playwright"
    ) {
      if (!Array.isArray(surface.specs) || surface.specs.length === 0) {
        errors.push(`coverage-matrix.json: surface '${tag}' (${surface.kind}) has no specs`);
      } else {
        for (const spec of surface.specs) {
          const specPath = resolve(webDir, spec);
          if (!(await pathExists(specPath))) {
            errors.push(`coverage-matrix.json: surface '${tag}' references missing spec '${spec}'`);
          }
        }
      }
    } else {
      errors.push(`coverage-matrix.json: surface '${tag}' has unknown kind '${surface.kind}'`);
    }
    for (const c of surface.components ?? []) {
      seenComponents.add(c);
    }
  }

  // Walk the components tree and check every .tsx is referenced.
  const all = await walkTsx(componentsRoot);
  for (const abs of all) {
    const rel = normalize(abs);
    if (seenComponents.has(rel)) continue;
    if (exemptPaths.has(rel)) continue;
    errors.push(
      `Component '${rel}' is not listed in any coverage-matrix surface or coverage-matrix.exempt.json. ` +
        `Add it to a surface's components[] or to the exempt list with a one-line reason.`,
    );
  }

  if (errors.length > 0) {
    console.error("Coverage matrix validation failed:\n");
    for (const e of errors) console.error(`  - ${e}`);
    console.error(`\n${errors.length} issue(s).`);
    process.exit(1);
  }
  console.log("Coverage matrix validation passed.");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
