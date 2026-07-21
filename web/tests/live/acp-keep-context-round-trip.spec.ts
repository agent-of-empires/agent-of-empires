// #2252: context-preserving view switch, both directions, end to end.
//
// Seeds a claude transcript on disk, imports it into a structured session
// (history replays), then round-trips:
//   structured --disable--> terminal  (direction A: acp_session_id carried into
//                                       agent_session_id, `claude --resume`)
//   terminal   --enable--> structured (direction B: agent_session_id fed back
//                                       into session/load, history replays)
// The transcript is written at the Claude-encoded project path so the
// direction-B host-transcript-present gate fires (a missing transcript would
// hard-fail the seeded load, so the gate skips it).

import { mkdirSync, realpathSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions } from "../helpers/aoeServe";

const SID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const REPLAY_TEXT = "round-trip transcript line qwerty";
const PROJECT_SUBDIR = "kc-project";

// Mirror src/session/capture.rs encode_claude_project_path over the canonical
// path (std::fs::canonicalize == realpathSync): non-alphanumeric (except '-')
// becomes '-'.
function encodeClaudeProjectPath(canonicalPath: string): string {
  return canonicalPath.replace(/[^a-zA-Z0-9-]/g, "-");
}

test("view switch preserves the claude conversation in both directions", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    extraEnv: { FAKE_ACP_LOAD_REPLAY: REPLAY_TEXT },
    seedFn: ({ home }) => {
      const projectDir = join(home, PROJECT_SUBDIR);
      mkdirSync(projectDir, { recursive: true });
      // Place the transcript at the encoded canonical path so both the import
      // scanner (reads cwd from content) and the direction-B presence check
      // (looks under the encoded path) find it.
      const encoded = encodeClaudeProjectPath(realpathSync(projectDir));
      const claudeProjects = join(home, ".claude", "projects", encoded);
      mkdirSync(claudeProjects, { recursive: true });
      const line = JSON.stringify({
        type: "user",
        cwd: projectDir,
        message: { role: "user", content: [{ type: "text", text: "round-trip prompt" }] },
      });
      writeFileSync(join(claudeProjects, `${SID}.jsonl`), `${line}\n`);
    },
  });

  const replayContains = async (sessionId: string, needle: string) =>
    expect
      .poll(
        async () => {
          const res = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`);
          if (!res.ok) return "";
          const body = await res.json();
          return JSON.stringify(body.frames ?? []);
        },
        { timeout: 20_000, intervals: [200, 500, 1000] },
      )
      .toContain(needle);

  try {
    const projectDir = join(serve.home, PROJECT_SUBDIR);

    // Import the seeded transcript into a structured session (history replays).
    const createRes = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: projectDir, tool: "claude", title: "kc", import_acp_session_id: SID }),
    });
    expect(createRes.ok, `create failed: ${createRes.status}`).toBe(true);
    const sessionId: string = (await createRes.json()).id;
    await replayContains(sessionId, REPLAY_TEXT);

    // Direction A: structured -> terminal, keep context.
    const disableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/disable`, { method: "POST" });
    expect(disableRes.ok).toBe(true);
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.view === "structured", {
        timeout: 10_000,
        intervals: [100, 200, 400],
      })
      .toBe(false);

    // Direction B: terminal -> structured, keep context. The carried
    // agent_session_id drives a seeded session/load; the transcript replays.
    const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableRes.ok).toBe(true);
    await replayContains(sessionId, REPLAY_TEXT);

    // The reloaded structured session adopts the same acp_session_id.
    await expect
      .poll(
        async () =>
          (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId) as
            | { acp_session_id?: string }
            | undefined,
        { timeout: 10_000, intervals: [100, 200, 400] },
      )
      .toMatchObject({ acp_session_id: SID });
  } finally {
    await serve.stop();
  }
});
