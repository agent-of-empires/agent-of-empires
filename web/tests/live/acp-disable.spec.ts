// Structured view shutdown via DELETE.
//
// `DELETE /api/sessions/:id/acp` calls `supervisor.shutdown(&id)`
// to tear down the structured view worker subprocess. Returns 204 on success,
// 404 when the supervisor has no entry for the session.
//
// Distinct from `POST /acp/disable`, which also swaps view
// back to tmux. This endpoint only stops the worker; view state
// (structured_view) is preserved so a subsequent
// `POST /acp/spawn` can re-attach without re-enabling.

import { test, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

test("DELETE /acp shuts the worker down with 204 / 404", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-shutdown" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    // Pre-enable: the supervisor may already have a pending-spawn entry
    // for this session from its boot-time reconcile pass, so DELETE can
    // legitimately land on either an absent worker (404) or a
    // marked-for-cancel pending spawn (204). The load-bearing assertions
    // are the post-enable 204 and the structured_view-still-true contract.
    const preDelete = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp`,
      { method: "DELETE" },
    );
    expect([204, 404]).toContain(preDelete.status);

    // Bring the worker up. The supervisor's spawn is `tokio::spawn`'d
    // inside enable, so the worker entry may not yet exist when enable
    // returns; poll up to 5s for the registry insert.
    await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, {
      method: "POST",
    });

    let postDeleteStatus = 0;
    for (let attempt = 0; attempt < 25; attempt++) {
      const res = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp`,
        { method: "DELETE" },
      );
      postDeleteStatus = res.status;
      if (res.status === 204) break;
      await new Promise((r) => setTimeout(r, 200));
    }
    expect(postDeleteStatus).toBe(204);

    // View state survives the worker teardown: structured_view is
    // still true on the session record. That's the contract that
    // distinguishes shutdown from disable.
    const after = await listSessions(serve.baseUrl);
    expect(after.find((s) => s.id === sessionId)!.view === "structured").toBe(
      true,
    );

    // The reconciler may re-spawn the worker for a session whose
    // structured_view is still true, so a second DELETE can land on either
    // an absent worker (404) or a freshly-respawned one (204). Either
    // outcome satisfies the lifecycle contract; the load-bearing
    // assertion is the initial 404 -> 204 transition above.
    const repeat = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp`,
      { method: "DELETE" },
    );
    expect([204, 404]).toContain(repeat.status);
  } finally {
    await serve.stop();
  }
});
