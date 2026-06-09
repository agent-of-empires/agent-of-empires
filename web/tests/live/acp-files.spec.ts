// Structured view @-mention file index.
//
// `GET /api/sessions/:id/acp/files` walks the session's project_path
// (capped at 5k entries, skipping common VCS / build dirs) and returns
// `{ files: string[], truncated: boolean }`. This spec seeds a project
// with a known shape, hits the endpoint, and asserts the shape comes
// back. Independent of the structured view supervisor: the handler talks straight
// to the filesystem, so it works even when #1237 keeps the ACP
// supervisor parked.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

test("structured view/files lists workspace files and honors the skip rules", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: (env) => {
      // Default seed sets up project + commit; then layer our own
      // fixture tree on top so we get deterministic file names.
      seedSessionViaAoeAdd({ title: "acp-files" })(env);
      const projectDir = join(env.home, "project");
      writeFileSync(join(projectDir, "main.rs"), "fn main() {}\n");
      mkdirSync(join(projectDir, "src"), { recursive: true });
      writeFileSync(join(projectDir, "src", "lib.rs"), "// lib\n");
      mkdirSync(join(projectDir, "src", "nested"), { recursive: true });
      writeFileSync(join(projectDir, "src", "nested", "deep.rs"), "// deep\n");
      // SKIP_DIRS entry: must NOT appear in the response.
      mkdirSync(join(projectDir, "node_modules", "junk"), { recursive: true });
      writeFileSync(join(projectDir, "node_modules", "junk", "ignore.js"), "// ignore\n");
      // Dot-file at top level: must NOT appear.
      writeFileSync(join(projectDir, ".secret"), "should be hidden\n");
    },
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    expect(sessions.length).toBeGreaterThan(0);
    const sessionId = sessions[0]!.id;

    const res = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/files`);
    expect(res.ok).toBeTruthy();
    const body = (await res.json()) as { files: string[]; truncated: boolean };
    expect(Array.isArray(body.files)).toBe(true);
    expect(body.truncated).toBe(false);

    // Files we expect to appear (paths are relative to project_path).
    expect(body.files).toContain("main.rs");
    expect(body.files).toContain("src/lib.rs");
    expect(body.files).toContain("src/nested/deep.rs");
    // SKIP_DIRS contents must be filtered.
    expect(body.files.some((f) => f.startsWith("node_modules/"))).toBe(false);
    // Top-level dot-files must be filtered.
    expect(body.files).not.toContain(".secret");

    // Unknown session id returns 404.
    const notFound = await fetch(`${serve.baseUrl}/api/sessions/does-not-exist/acp/files`);
    expect(notFound.status).toBe(404);
  } finally {
    await serve.stop();
  }
});
