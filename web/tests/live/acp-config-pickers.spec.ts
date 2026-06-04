// Structured view model picker + reasoning effort selector (#1403).
//
// POST /api/sessions/:id/acp/config-option forwards to the fake
// ACP agent's `session/set_config_option` handler, which emits a
// follow-up `config_option_update` notification. The structured view reducer
// records both the initial snapshot (emitted on session/new) and the
// post-set update via the replay endpoint. Mirrors
// acp-mode-switch.spec.ts.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

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

async function waitForReplay(
  baseUrl: string,
  sessionId: string,
  predicate: (replayJson: string) => boolean,
  maxAttempts = 30,
): Promise<boolean> {
  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    const replay = await fetch(
      `${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`,
    ).then((r) => r.json());
    const json = JSON.stringify(replay);
    if (predicate(json)) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}

base(
  "initial config_option_update from the adapter lands in structured view replay",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "config-pickers-initial" }),
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      const saw = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (json) =>
          json.includes("ConfigOptionsUpdated") &&
          json.includes("claude-opus-4-7") &&
          json.includes("thought_level"),
      );
      expect(saw).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "session/set_config_option round-trips and the adapter confirms with a new snapshot",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "config-pickers-set" }),
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      // Wait for the initial snapshot so we know the supervisor saw
      // session/new complete before issuing set_config_option.
      const sawInitial = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (json) => json.includes("ConfigOptionsUpdated"),
      );
      expect(sawInitial).toBe(true);

      const setRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/config-option`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            config_id: "model",
            value: "claude-sonnet-4-6",
          }),
        },
      );
      expect(setRes.status).toBeGreaterThanOrEqual(200);
      expect(setRes.status).toBeLessThan(300);

      // Confirming snapshot from the adapter carries the requested
      // value as the new current_value. The replay endpoint should
      // contain at least one ConfigOptionsUpdated event whose payload
      // includes `claude-sonnet-4-6`.
      const sawConfirm = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (json) =>
          json.includes("ConfigOptionsUpdated") &&
          json.includes("claude-sonnet-4-6"),
      );
      expect(sawConfirm).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "user switches reasoning effort to a valid level",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "config-pickers-effort" }),
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      const setRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/config-option`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ config_id: "effort", value: "high" }),
        },
      );
      expect(setRes.status).toBeGreaterThanOrEqual(200);
      expect(setRes.status).toBeLessThan(300);

      const sawConfirm = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (json) =>
          json.includes("ConfigOptionsUpdated") &&
          /"effort"[^}]*"current_value"\s*:\s*"high"/.test(json),
      );
      expect(sawConfirm).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "rejected set_config_option surfaces as ConfigOptionSwitchFailed",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "config-pickers-reject" }),
      extraEnv: { FAKE_ACP_REJECT_CONFIG_OPTION: "rate limited (test)" },
    });
    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;
      await enableAndSpawn(serve.baseUrl, sessionId);

      const setRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/config-option`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            config_id: "model",
            value: "claude-sonnet-4-6",
          }),
        },
      );
      // The HTTP path itself succeeds (the request is sent); the
      // rejection arrives as an async ConfigOptionSwitchFailed event
      // on the broadcast bus.
      expect(setRes.status).toBeGreaterThanOrEqual(200);
      expect(setRes.status).toBeLessThan(300);

      const sawFailure = await waitForReplay(
        serve.baseUrl,
        sessionId,
        (json) =>
          json.includes("ConfigOptionSwitchFailed") &&
          json.includes("rate limited"),
      );
      expect(sawFailure).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);
