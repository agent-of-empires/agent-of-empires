// Live: a claude `/clear` drives a REAL conversation reset and persists the
// new resume id, so the post-clear conversation survives a worker restart.
//
// claude-agent-acp DOES handle `/clear` locally, so the old text-forward path
// reset the model context correctly. What it never did was rotate the ACP
// session id: the adapter keeps serving the pre-clear id and discards the
// `conversation_reset` message carrying the new conversation id (upstream
// #906). #3083 responded by dropping the stored id on `SessionCleared`, which
// stopped the pre-clear conversation from resurrecting but left the post-clear
// one unresumable. Observed in the wild as a session that worked for two hours
// after a `/clear`, idled out overnight, and came back with no context at all.
//
// The server now drives the reset itself. This spec asserts the full contract
// against a real `aoe serve` plus the fake claude ACP agent:
//
//   - the clear boundary (`SessionCleared`) still renders,
//   - the reset boundary (`SessionContextReset`) fires,
//   - a SECOND `AcpSessionAssigned` lands with a NEW acp session id (proof a
//     fresh session/new ran rather than the alias being forwarded),
//   - and, the part the incident turned on, that new id is what ends up
//     PERSISTED in sessions.json, because that on-disk value is the only
//     handle a later respawn can feed to session/load.
//
// The persisted-id assertion is the actual regression guard. Everything above
// it was already true of a forwarded `/clear` from the UI's point of view,
// which is precisely why the data loss went unnoticed.
//
// API-only (no page): the routing decision is unit-tested in
// `src/acp/supervisor.rs`, the profile table in `src/acp/agent_profiles.rs`.

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd, appDirFor, resolveAoeBinary } from "../helpers/aoeServe";

test("claude /clear opens a fresh session/new and persists the new acp session id", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "claude-clear-reset", tool: "claude" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    const replayFrames = async (): Promise<unknown[]> => {
      const replay = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
      return replay.frames ?? [];
    };
    // An event is either a bare string for a unit variant (`"SessionCleared"`)
    // or a single-key object (`{ SessionContextReset: { reason } }`).
    type FrameEvent = string | Record<string, { acp_session_id?: string; reason?: string }>;
    const eventOf = (f: unknown): FrameEvent | undefined => (f as { event?: FrameEvent }).event;
    const kindOf = (ev: FrameEvent | undefined): string | undefined =>
      typeof ev === "string" ? ev : ev ? Object.keys(ev)[0] : undefined;
    const payloadOf = (ev: FrameEvent | undefined) =>
      typeof ev === "string" || !ev ? undefined : Object.values(ev)[0];

    const assignedIds = async (): Promise<string[]> => {
      const ids: string[] = [];
      for (const f of await replayFrames()) {
        const ev = eventOf(f);
        if (kindOf(ev) === "AcpSessionAssigned") {
          const id = payloadOf(ev)?.acp_session_id;
          if (id) ids.push(id);
        }
      }
      return ids;
    };
    /**
     * The ordered contract of a successful driven reset, by frame index rather
     * than substring match: `SessionCleared`, then `SessionContextReset`, then
     * a SECOND `AcpSessionAssigned`, then a terminal `Stopped(session_reset)`.
     *
     * Order is the load-bearing part, not just presence. The server's listener
     * folds `SessionCleared` and `SessionContextReset` to a null stored id and
     * `AcpSessionAssigned` to the new one, so the assignment landing LAST is the
     * only reason the fresh id is what ends up persisted.
     */
    const resetSequenceComplete = async (): Promise<boolean> => {
      const kinds = (await replayFrames()).map((f) => ({ kind: kindOf(eventOf(f)), payload: payloadOf(eventOf(f)) }));
      const cleared = kinds.findIndex((k) => k.kind === "SessionCleared");
      if (cleared < 0) return false;
      const reset = kinds.findIndex((k, i) => i > cleared && k.kind === "SessionContextReset");
      if (reset < 0) return false;
      const assigned = kinds.findIndex((k, i) => i > reset && k.kind === "AcpSessionAssigned");
      if (assigned < 0) return false;
      return kinds.some((k, i) => i > assigned && k.kind === "Stopped" && k.payload?.reason === "session_reset");
    };
    // `acp_session_id` is not exposed over the REST API, so read the same
    // on-disk record a respawn would read. The harness roots the daemon at an
    // isolated HOME, so this is the test's own sessions.json, not the user's.
    //
    // The profile directory name is resolved (or bootstrapped) by
    // `config::resolve_default_profile`, so it is not reliably "default";
    // scan `profiles/*/sessions.json` for the one holding this session.
    const persistedAcpSessionId = (): string | undefined => {
      const appDir = appDirFor(serve.home, serve.env.XDG_CONFIG_HOME!, resolveAoeBinary());
      const profilesDir = join(appDir, "profiles");
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

    await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });

    // Handshake oracle: the first AcpSessionAssigned frame proves the fake
    // claude worker is live and its initial session/new completed.
    await expect
      .poll(async () => (await assignedIds()).length, { timeout: 45_000, intervals: [500] })
      .toBeGreaterThan(0);
    const firstId = (await assignedIds())[0]!;

    // `/clear` goes through the ordinary prompt endpoint; the server-side clear
    // detection reroutes it onto the driven-reset path. The assigned id above
    // proves the worker is ready, so submit this stateful request exactly once.
    const resetResponse = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "/clear" }),
    });
    expect(resetResponse.status).toBe(202);

    // The reset's full ordered event contract in one poll.
    await expect.poll(resetSequenceComplete, { timeout: 45_000, intervals: [500] }).toBe(true);

    const ids = await assignedIds();
    const freshId = ids[ids.length - 1]!;
    expect(freshId, "the reset must mint a NEW acp session id via session/new").not.toBe(firstId);

    // The regression guard. Pre-fix this settled on `null`, because
    // `SessionCleared` nulled the stored id and a forwarded `/clear` never
    // produced an `AcpSessionAssigned` to re-pin it. A respawn then had nothing
    // to resume and silently started an empty conversation.
    await expect.poll(persistedAcpSessionId, { timeout: 15_000, intervals: [250] }).toBe(freshId);
  } finally {
    await serve.stop();
  }
});
