// Import an existing Claude Code session into a structured-view session
// (#2276), end to end through a real `aoe serve`.
//
// Seeds a Claude Code transcript on disk (~/.claude/projects/.../<id>.jsonl)
// with a known cwd, then verifies:
//   - GET /api/claude-sessions discovers it (id, cwd, title, cwd_exists)
//   - POST /api/sessions with import_acp_session_id creates a structured
//     session in that cwd and resumes the id via session/load
//   - the resumed transcript is seeded into the event store and replays
//     (proving the import path does NOT suppress history the way a normal
//     reattach does). The fake agent emits a deterministic load-replay
//     chunk via FAKE_ACP_LOAD_REPLAY.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { test, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

const IMPORT_SID = "11111111-2222-3333-4444-555555555555";
const REPLAY_TEXT = "imported transcript line abc123";
const PROJECT_SUBDIR = "imported-project";

test("imports an existing Claude session and replays its transcript", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    extraEnv: { FAKE_ACP_LOAD_REPLAY: REPLAY_TEXT },
    seedFn: ({ home }) => {
      const projectDir = join(home, PROJECT_SUBDIR);
      mkdirSync(projectDir, { recursive: true });
      // The scanner reads cwd from the transcript, not the (lossy) encoded
      // directory name, so the project subdir name is irrelevant here.
      const claudeProjects = join(home, ".claude", "projects", "imported-proj");
      mkdirSync(claudeProjects, { recursive: true });
      const line = JSON.stringify({
        type: "user",
        cwd: projectDir,
        message: { role: "user", content: [{ type: "text", text: "Imported session prompt" }] },
      });
      writeFileSync(join(claudeProjects, `${IMPORT_SID}.jsonl`), `${line}\n`);
    },
  });

  try {
    const projectDir = join(serve.home, PROJECT_SUBDIR);

    // 1. Discovery endpoint lists the seeded session.
    const listRes = await fetch(`${serve.baseUrl}/api/claude-sessions`);
    expect(listRes.ok).toBe(true);
    const sessions: {
      session_id: string;
      cwd: string;
      title: string | null;
      cwd_exists: boolean;
    }[] = await listRes.json();
    const found = sessions.find((s) => s.session_id === IMPORT_SID);
    expect(found, "seeded session should be discovered").toBeTruthy();
    expect(found!.cwd).toBe(projectDir);
    expect(found!.cwd_exists).toBe(true);
    expect(found!.title).toBe("Imported session prompt");

    // 2. Create a structured session importing it.
    const createRes = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        path: projectDir,
        tool: "claude",
        title: "imported",
        import_acp_session_id: IMPORT_SID,
      }),
    });
    expect(createRes.ok, `create failed: ${createRes.status}`).toBe(true);
    const created = await createRes.json();
    const newId: string = created.id;
    expect(newId).toBeTruthy();
    // The session adopts the imported id and renders in the structured view.
    expect(created.view).toBe("structured");

    // 3. The resumed transcript is seeded into the store (not suppressed).
    await expect
      .poll(
        async () => {
          const res = await fetch(`${serve.baseUrl}/api/sessions/${newId}/acp/replay?since=0`);
          if (!res.ok) return "";
          const body = await res.json();
          return JSON.stringify(body.frames ?? []);
        },
        { timeout: 20_000, intervals: [200, 500, 1000] },
      )
      .toContain(REPLAY_TEXT);
  } finally {
    await serve.stop();
  }
});
