// Structured view WebSocket lagged-channel signaling.
//
// When the broadcast channel overflows (capacity 256, see
// `ACP_CHANNEL_CAPACITY` in `src/server/mod.rs`), the per-client
// receiver gets `RecvError::Lagged(skipped)` and the WS handler sends
// `{ kind: "lagged", skipped: N }` so the client knows to request a
// snapshot+replay rather than silently diverging
// (`src/server/acp_ws.rs:199`).
//
// Skipped pending #1237. Driving >256 events fast enough to overflow
// the channel requires the prompt path to work (the fake ACP agent's
// session/prompt handler emits scripted updates rapidly enough to
// saturate the channel when the WS receiver is paused); today that
// path surfaces `AgentStartupError "Authentication required"` before
// any updates land. Until #1237 is fixed, neither the flood nor the
// pause-then-overflow sequence yields a deterministic test.

import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableStructuredViewAndWait } from "../helpers/acp";

const FLOOD_UPDATES = Array.from({ length: 300 }, (_, i) => ({
  sessionUpdate: "agent_message_chunk",
  content: { type: "text", text: `chunk ${i}` },
}));

const FLOOD_SCRIPT = {
  turns: [
    {
      updates: FLOOD_UPDATES,
      stopReason: "end_turn",
    },
  ],
};

base.skip(
  "lagged WS receiver gets a kind:lagged frame from the server",
  async ({}, testInfo) => {
    const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-lagged-"));
    const scriptPath = join(scriptDir, "script.json");
    writeFileSync(scriptPath, JSON.stringify(FLOOD_SCRIPT));

    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "acp-lagged" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId = sessions[0]!.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      // Sketch only. The full version would: (1) open a WebSocket to
      // /sessions/:id/acp/ws and stop reading its frames so the
      // per-client receiver lags, (2) POST /acp/prompt to trigger the
      // 300-update flood, (3) resume reading and assert a
      // `{ kind: "lagged", skipped: N }` frame appears before normal
      // delivery resumes. The above is unreachable today because the
      // flood never starts; the supervisor publishes AgentStartupError
      // before the scripted updates emit.
      await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text: "trigger flood" }),
      });
      expect(true).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);
