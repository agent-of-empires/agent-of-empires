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
const WORKTREE_SID = "22222222-3333-4444-5555-666666666666";
const WORKSPACE_SID = "33333333-4444-5555-6666-777777777777";
const SCRATCH_SID = "44444444-5555-6666-7777-888888888888";
const REPLAY_TEXT = "imported transcript line abc123";
const REPLAY_USER_TEXT = "the original user question xyz789";
const PROJECT_SUBDIR = "imported-project";

test("imports an existing Claude session and replays its transcript", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    extraEnv: { FAKE_ACP_LOAD_REPLAY: REPLAY_TEXT, FAKE_ACP_LOAD_REPLAY_USER: REPLAY_USER_TEXT },
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

      // A Claude session living inside an AoE worktree dir (matching the
      // "*-worktrees" path template) must never be offered for import.
      const worktreeCwd = join(home, "agent-of-empires-worktrees", "Saracens");
      mkdirSync(worktreeCwd, { recursive: true });
      const wtLine = JSON.stringify({
        type: "user",
        cwd: worktreeCwd,
        message: { role: "user", content: [{ type: "text", text: "Base directory for this skill" }] },
      });
      writeFileSync(join(claudeProjects, `${WORKTREE_SID}.jsonl`), `${wtLine}\n`);

      // A multi-repo workspace session carries the marker mid-name
      // (<branch>-workspace-<id>); it must also be excluded.
      const workspaceCwd = join(home, "feat-mm-template-sending-workspace-b13b3665");
      mkdirSync(workspaceCwd, { recursive: true });
      const wsLine = JSON.stringify({
        type: "user",
        cwd: workspaceCwd,
        message: { role: "user", content: [{ type: "text", text: "plan then implement" }] },
      });
      writeFileSync(join(claudeProjects, `${WORKSPACE_SID}.jsonl`), `${wsLine}\n`);

      // A scratch session in the *other* (release) namespace must be excluded
      // too: the feature does not discriminate dev vs release. The serving
      // daemon's app dir differs from this hardcoded `.agent-of-empires` path.
      const scratchCwd = join(home, ".agent-of-empires", "scratch", "5c8d250f60ec4328");
      mkdirSync(scratchCwd, { recursive: true });
      const scLine = JSON.stringify({
        type: "user",
        cwd: scratchCwd,
        message: { role: "user", content: [{ type: "text", text: "Generate a concise title" }] },
      });
      writeFileSync(join(claudeProjects, `${SCRATCH_SID}.jsonl`), `${scLine}\n`);
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
    // AoE worktree / workspace / scratch (any namespace) sessions are excluded.
    expect(sessions.some((s) => s.session_id === WORKTREE_SID)).toBe(false);
    expect(sessions.some((s) => s.session_id === WORKSPACE_SID)).toBe(false);
    expect(sessions.some((s) => s.session_id === SCRATCH_SID)).toBe(false);

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

    // 3. The resumed transcript is seeded into the store (not suppressed),
    // including the replayed USER turn (user_message_chunk -> UserPromptSent,
    // #2276) so the imported conversation isn't missing the user's messages.
    let frames = "";
    await expect
      .poll(
        async () => {
          const res = await fetch(`${serve.baseUrl}/api/sessions/${newId}/acp/replay?since=0`);
          if (!res.ok) return "";
          const body = await res.json();
          frames = JSON.stringify(body.frames ?? []);
          return frames;
        },
        { timeout: 20_000, intervals: [200, 500, 1000] },
      )
      .toContain(REPLAY_TEXT);
    // The user turn rendered as a UserPromptSent event carrying its text.
    expect(frames).toContain(REPLAY_USER_TEXT);
    expect(frames).toContain("UserPromptSent");

    // 4. Now that AoE manages this session by id (acp_session_id), it drops
    // out of the import list so it is not offered for re-import. (Filtering a
    // plain project_path is deliberately NOT done, so an external claude
    // session in the same repo would still show; only the managed id is hidden.)
    await expect
      .poll(
        async () => {
          const res = await fetch(`${serve.baseUrl}/api/claude-sessions`);
          if (!res.ok) return true;
          const list: { session_id: string }[] = await res.json();
          return list.some((s) => s.session_id === IMPORT_SID);
        },
        { timeout: 10_000, intervals: [200, 500, 1000] },
      )
      .toBe(false);

    // 5. An import that pairs a real id with the wrong cwd is rejected, so a
    // stale/hand-written request can't seed the transcript in the wrong place.
    const badRes = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        path: join(serve.home, "some-other-dir"),
        tool: "claude",
        import_acp_session_id: WORKTREE_SID,
      }),
    });
    expect(badRes.status).toBe(400);
  } finally {
    await serve.stop();
  }
});
