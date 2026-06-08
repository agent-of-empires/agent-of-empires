// Structured view model picker driven by the ACP unstable_session_model
// channel (#1820).
//
// When an agent advertises its model selector via SessionModelState on
// the session/new response (instead of a generic config_option), aoe
// normalizes it into a synthetic model config option (reserved id
// `__aoe_acp_session_model__`) so the existing model dropdown renders
// unchanged. The acp/config-option endpoint with that id routes to
// ACP `session/set_model`, and aoe synthesizes the confirming snapshot
// since the adapter only acks. The fake agent emits the unstable
// channel under FAKE_ACP_EMIT_SESSION_MODEL=1 and suppresses the
// config_option model with FAKE_ACP_EMIT_CONFIG_OPTIONS=0. Mirrors
// acp-config-pickers.spec.ts.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

const SYNTHETIC_MODEL_ID = "__aoe_acp_session_model__";

async function enableAndSpawn(baseUrl: string, sessionId: string) {
  const enableRes = await fetch(
    `${baseUrl}/api/sessions/${sessionId}/acp/enable`,
    { method: "POST" },
  );
  expect(enableRes.ok).toBeTruthy();
  const spawnRes = await fetch(
    `${baseUrl}/api/sessions/${sessionId}/acp/spawn`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agent: "claude" }),
    },
  );
  expect([200, 202, 409]).toContain(spawnRes.status);
}

type Json = Record<string, unknown>;

async function waitForReplay(
  baseUrl: string,
  sessionId: string,
  predicate: (replay: Json) => boolean,
  maxAttempts = 30,
): Promise<boolean> {
  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    const replay = (await fetch(
      `${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`,
    ).then((r) => r.json())) as Json;
    if (predicate(replay)) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}

/** Externally-tagged structured view events of `kind` pulled out of the replay's
 *  typed `frames`, so assertions inspect the specific event payload rather
 *  than substring-matching one big JSON blob. */
function frameEvents(replay: Json, kind: string): Json[] {
  const frames = (replay.frames as Array<{ event?: Json }>) ?? [];
  return frames
    .map((f) => f.event?.[kind])
    .filter((e): e is Json => e != null && typeof e === "object");
}

/** The synthetic model selector inside a `ConfigOptionsUpdated` payload. */
function modelOption(event: Json): Json | undefined {
  const options = (event.options as Json[]) ?? [];
  return options.find((o) => o.id === SYNTHETIC_MODEL_ID);
}

function modelChoiceValues(event: Json): string[] {
  const model = modelOption(event);
  if (!model) return [];
  return ((model.options as Json[]) ?? []).map((c) => String(c.value));
}

const unstableModelEnv = {
  FAKE_ACP_EMIT_CONFIG_OPTIONS: "0",
  FAKE_ACP_EMIT_SESSION_MODEL: "1",
};

base(
  "unstable session_model surfaces as a synthetic model selector",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "session-model-initial" }),
      extraEnv: unstableModelEnv,
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      const saw = await waitForReplay(serve.baseUrl, sessionId, (replay) =>
        frameEvents(replay, "ConfigOptionsUpdated").some((e) => {
          const values = modelChoiceValues(e);
          return values.includes("fake-sonnet") && values.includes("fake-opus");
        }),
      );
      expect(saw).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "selecting the unstable model routes through session/set_model",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "session-model-set" }),
      extraEnv: unstableModelEnv,
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      const sawInitial = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (replay) =>
          frameEvents(replay, "ConfigOptionsUpdated").some((e) =>
            modelChoiceValues(e).includes("fake-sonnet"),
          ),
      );
      expect(sawInitial).toBe(true);

      const setRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/config-option`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            config_id: SYNTHETIC_MODEL_ID,
            value: "fake-opus",
          }),
        },
      );
      expect(setRes.status).toBeGreaterThanOrEqual(200);
      expect(setRes.status).toBeLessThan(300);

      // The fake only acks set_model; aoe synthesizes the confirming
      // snapshot from the cached state, so the synthetic selector's
      // current_value must flip to the requested model.
      const sawConfirm = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (replay) =>
          frameEvents(replay, "ConfigOptionsUpdated").some(
            (e) => modelOption(e)?.current_value === "fake-opus",
          ),
      );
      expect(sawConfirm).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "rejected set_model surfaces as ConfigOptionSwitchFailed",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "session-model-reject" }),
      extraEnv: {
        ...unstableModelEnv,
        FAKE_ACP_REJECT_SET_MODEL: "model unavailable (test)",
      },
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      const sawInitial = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (replay) =>
          frameEvents(replay, "ConfigOptionsUpdated").some(
            (e) => modelOption(e) != null,
          ),
      );
      expect(sawInitial).toBe(true);

      const setRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/config-option`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            config_id: SYNTHETIC_MODEL_ID,
            value: "fake-opus",
          }),
        },
      );
      expect(setRes.status).toBeGreaterThanOrEqual(200);
      expect(setRes.status).toBeLessThan(300);

      const sawFailure = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (replay) =>
          frameEvents(replay, "ConfigOptionSwitchFailed").some(
            (e) =>
              e.config_id === SYNTHETIC_MODEL_ID &&
              String(e.reason).includes("model unavailable"),
          ),
      );
      expect(sawFailure).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);
