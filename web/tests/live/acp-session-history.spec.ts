// Conversation identity: driven resets, importing Claude sessions, and keeping context across view switches.

import { existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { postAcp, postPrompt, replayFrames, startAcpSession, waitForReplayContains } from "../helpers/acp";

// A unit variant serializes as a bare string, others as a single-key object.
type FrameEvent = string | Record<string, { acp_session_id?: string }>;
const eventsOf = async (serve: ServeHandle, sessionId: string) =>
  ((await replayFrames(serve.baseUrl, sessionId)) as { event?: FrameEvent }[]).map(({ event }) => ({
    kind: typeof event === "string" ? event : event ? Object.keys(event)[0] : undefined,
    payload: typeof event === "string" || !event ? undefined : Object.values(event)[0],
  }));
const assignedIds = async (serve: ServeHandle, sessionId: string) =>
  (await eventsOf(serve, sessionId)).flatMap((e) =>
    e.kind === "AcpSessionAssigned" && e.payload?.acp_session_id ? [e.payload.acp_session_id] : [],
  );

// Reset creates a fresh native session and persists its assignment.
for (const c of [
  { tool: "claude", command: "/clear", claude: true },
  // #2979: codex-acp has no native /new, so a forwarded alias was swallowed as an unknown command.
  { tool: "codex", command: "/new", claude: false },
]) {
  test(`${c.tool} ${c.command} opens a fresh session/new and swaps the acp session id`, async ({ spawnServe }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, { title: `${c.tool}-reset`, tool: c.tool });
    // The first assignment proves the worker's initial session/new completed; the reset is submitted once.
    await expect
      .poll(async () => (await assignedIds(serve, sessionId)).length, { timeout: 45_000, intervals: [500] })
      .toBeGreaterThan(0);
    const [firstId] = await assignedIds(serve, sessionId);
    expect((await postPrompt(serve.baseUrl, sessionId, c.command)).status).toBe(202);

    const resetDone = async () => {
      const freshId = (await assignedIds(serve, sessionId)).at(-1);
      const session = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId);
      return Boolean(freshId && freshId !== firstId && session?.acp_session_id === freshId);
    };
    await expect.poll(resetDone, { timeout: 45_000, intervals: [500] }).toBe(true);
    const freshId = (await assignedIds(serve, sessionId)).at(-1);
    expect(freshId, "the reset must mint a NEW acp session id via session/new").not.toBe(firstId);
    expect(freshId).toBeTruthy();
    expect(await assignedIds(serve, sessionId)).toContain(firstId);
    const nextInput = `${c.tool} input after reset`;
    expect((await postPrompt(serve.baseUrl, sessionId, nextInput)).status).toBe(202);
    await waitForReplayContains(serve.baseUrl, sessionId, nextInput);
    expect((await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)).toMatchObject({
      view: "structured",
      acp_session_id: freshId,
    });

    if (!c.claude) {
      expect(JSON.stringify(await replayFrames(serve.baseUrl, sessionId))).not.toContain("unknown command");
    } else {
      // Regression guard: this settled on null, leaving a later respawn nothing to session/load.
      // Read disk independently of REST; the profile directory resolves at boot.
      const persistedAcpSessionId = () => {
        const profilesDir = join(appDirFor(serve.home, serve.env.XDG_CONFIG_HOME!, resolveAoeBinary()), "profiles");
        for (const profile of readdirSync(profilesDir)) {
          const path = join(profilesDir, profile, "sessions.json");
          if (!existsSync(path)) continue;
          const parsed: unknown = JSON.parse(readFileSync(path, "utf8"));
          const all = Array.isArray(parsed) ? parsed : ((parsed as { sessions?: unknown[] }).sessions ?? []);
          const inst = (all as { id?: string; acp_session_id?: string }[]).find((s) => s.id === sessionId);
          if (inst) return inst.acp_session_id;
        }
        return undefined;
      };
      await expect.poll(persistedAcpSessionId, { timeout: 15_000, intervals: [250] }).toBe(freshId);
    }
  });
}

/** Write a one-line Claude transcript under `~/.claude/projects/<dir>/<sid>.jsonl`. */
function seedTranscript(home: string, projectDirName: string, sid: string, cwd: string, text: string) {
  const dir = join(home, ".claude", "projects", projectDirName);
  mkdirSync(dir, { recursive: true });
  mkdirSync(cwd, { recursive: true });
  const line = JSON.stringify({ type: "user", cwd, message: { role: "user", content: [{ type: "text", text }] } });
  writeFileSync(join(dir, `${sid}.jsonl`), `${line}\n`);
}

const replayText = async (serve: ServeHandle, sessionId: string) => {
  const res = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`);
  return res.ok ? JSON.stringify((await res.json()).frames ?? []) : "";
};

async function importSession(serve: ServeHandle, body: Record<string, unknown>) {
  return fetch(`${serve.baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tool: "claude", ...body }),
  });
}

test("imports an existing Claude session and replays its transcript", async ({ spawnServe }) => {
  // #2276
  const sids = {
    imported: "11111111-2222-3333-4444-555555555555",
    worktree: "22222222-3333-4444-5555-666666666666",
    workspace: "33333333-4444-5555-6666-777777777777",
    scratch: "44444444-5555-6666-7777-888888888888",
  };
  const replay = "imported transcript line abc123";
  const replayUser = "the original user question xyz789";
  const serve = await spawnServe({
    acp: true,
    extraEnv: { FAKE_ACP_LOAD_REPLAY: replay, FAKE_ACP_LOAD_REPLAY_USER: replayUser },
    seedFn: ({ home }) => {
      // The scanner reads cwd from the transcript, so the encoded dir name is irrelevant here.
      seedTranscript(home, "imported-proj", sids.imported, join(home, "imported-project"), "Imported session prompt");
      // AoE worktree, multi-repo workspace, and scratch (either namespace) sessions are never offered.
      seedTranscript(
        home,
        "imported-proj",
        sids.worktree,
        join(home, "agent-of-empires-worktrees", "Saracens"),
        "Base directory for this skill",
      );
      seedTranscript(
        home,
        "imported-proj",
        sids.workspace,
        join(home, "feat-mm-template-sending-workspace-b13b3665"),
        "plan then implement",
      );
      seedTranscript(
        home,
        "imported-proj",
        sids.scratch,
        join(home, ".agent-of-empires", "scratch", "5c8d250f60ec4328"),
        "Generate a concise title",
      );
    },
  });
  const projectDir = join(serve.home, "imported-project");
  const claudeSessions = async () => {
    const res = await fetch(`${serve.baseUrl}/api/importable-sessions?agent=claude`);
    expect(res.ok).toBe(true);
    return (
      (await res.json()) as {
        sessions: { session_id: string; cwd: string; title: string | null; cwd_exists: boolean }[];
      }
    ).sessions;
  };

  const sessions = await claudeSessions();
  const found = sessions.find((s) => s.session_id === sids.imported);
  expect(found, "seeded session should be discovered").toBeTruthy();
  expect(found!.cwd).toBe(projectDir);
  expect(found!.cwd_exists).toBe(true);
  expect(found!.title).toBe("Imported session prompt");
  for (const excluded of [sids.worktree, sids.workspace, sids.scratch]) {
    expect(sessions.some((s) => s.session_id === excluded)).toBe(false);
  }

  const createRes = await importSession(serve, {
    path: projectDir,
    title: "imported",
    import_acp_session_id: sids.imported,
  });
  expect(createRes.ok, `create failed: ${createRes.status}`).toBe(true);
  const created = await createRes.json();
  expect(created.id).toBeTruthy();
  expect(created.view).toBe("structured");

  // Imported history replays, including the user turn as UserPromptSent.
  await expect
    .poll(() => replayText(serve, created.id), { timeout: 20_000, intervals: [200, 500, 1000] })
    .toContain(replay);
  const frames = await replayText(serve, created.id);
  expect(frames).toContain(replayUser);
  expect(frames).toContain("UserPromptSent");

  // A managed id drops out of the import list.
  await expect
    .poll(
      async () => {
        const res = await fetch(`${serve.baseUrl}/api/importable-sessions?agent=claude`);
        return (
          !res.ok ||
          ((await res.json()) as { sessions: { session_id: string }[] }).sessions.some(
            (s) => s.session_id === sids.imported,
          )
        );
      },
      {
        timeout: 10_000,
        intervals: [200, 500, 1000],
      },
    )
    .toBe(false);

  // A real id paired with the wrong cwd is rejected.
  const badRes = await importSession(serve, {
    path: join(serve.home, "some-other-dir"),
    import_acp_session_id: sids.worktree,
  });
  expect(badRes.status).toBe(400);
});

// #2252: switching views keeps the claude conversation.
test("view switch preserves the claude conversation in both directions", async ({ spawnServe }) => {
  const sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
  const replay = "round-trip transcript line qwerty";
  const serve = await spawnServe({
    acp: true,
    extraEnv: { FAKE_ACP_LOAD_REPLAY: replay },
    seedFn: ({ home }) => {
      const projectDir = join(home, "kc-project");
      mkdirSync(projectDir, { recursive: true });
      // Direction B only loads when the transcript exists at Claude's encoded canonical project path.
      const encoded = realpathSync(projectDir).replace(/[^a-zA-Z0-9-]/g, "-");
      seedTranscript(home, encoded, sid, projectDir, "round-trip prompt");
    },
  });
  const expectReplay = (sessionId: string) =>
    expect.poll(() => replayText(serve, sessionId), { timeout: 20_000, intervals: [200, 500, 1000] }).toContain(replay);

  const createRes = await importSession(serve, {
    path: join(serve.home, "kc-project"),
    title: "kc",
    import_acp_session_id: sid,
  });
  expect(createRes.ok, `create failed: ${createRes.status}`).toBe(true);
  const sessionId: string = (await createRes.json()).id;
  await expectReplay(sessionId);

  expect((await postAcp(serve.baseUrl, sessionId, "/disable")).ok).toBe(true);
  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.view === "structured", {
      timeout: 10_000,
      intervals: [100, 200, 400],
    })
    .toBe(false);

  // The carried agent_session_id drives a seeded session/load.
  expect((await postAcp(serve.baseUrl, sessionId, "/enable")).ok).toBe(true);
  await expectReplay(sessionId);
  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId), {
      timeout: 10_000,
      intervals: [100, 200, 400],
    })
    .toMatchObject({ acp_session_id: sid });
});

test("switch to terminal resumes the claude conversation (keep context)", async ({ spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "acp-keep-context", tool: "claude" });

  let acpSessionId = "";
  await expect
    .poll(
      async () => {
        const s = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId) as { acp_session_id?: string };
        acpSessionId = s?.acp_session_id ?? "";
        return acpSessionId;
      },
      { timeout: 15_000, intervals: [100, 200, 400] },
    )
    .not.toBe("");
  const session = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId);
  expect(typeof session?.project_path).toBe("string");
  const cwd = realpathSync(session!.project_path as string);
  seedTranscript(serve.home, cwd.replace(/[^a-zA-Z0-9-]/g, "-"), acpSessionId, cwd, "keep-context handoff");

  const disableRes = await postAcp(serve.baseUrl, sessionId, "/disable");
  expect(disableRes.ok).toBeTruthy();
  expect(((await disableRes.json()) as { view?: string }).view === "structured").toBe(false);

  // The shim logs argv as JSON; `--resume <id>` rather than the fresh-start `--session-id <id>`.
  const fakeLog = join(serve.home, "fake-acp.log");
  await expect
    .poll(() => (existsSync(fakeLog) ? readFileSync(fakeLog, "utf8") : ""), {
      timeout: 15_000,
      intervals: [100, 200, 400],
    })
    .toContain(`"--resume","${acpSessionId}"`);
});
