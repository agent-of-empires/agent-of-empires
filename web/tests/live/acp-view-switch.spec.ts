// Structured view view switch.
//
// `SwitchViewAction` toggles a session between tmux and structured view
// modes. The two endpoints (`POST /acp/enable` and
// `POST /acp/disable`) both return
// `{ session_id, view?: "structured" | "terminal" }` and persist the new view
// to the on-disk session record. This spec round-trips both directions
// and asserts the session-list reports the swap on each step.
//
// Independent of #1237: enable returns 200 even when the supervisor's
// async spawn later fails, and disable tears the worker down without
// going through the prompt path.

import { test, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

test("view switch round-trips between tmux and structured view", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-view" }),
  });

  try {
    const sessionsBefore = await listSessions(serve.baseUrl);
    const sessionId = sessionsBefore[0]!.id;
    // `aoe add` defaults to tmux mode.
    expect(sessionsBefore[0]!.view === "structured").toBeFalsy();

    // tmux -> structured view
    const enableRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`,
      { method: "POST" },
    );
    expect(enableRes.ok).toBeTruthy();
    const enableBody = (await enableRes.json()) as {
      session_id: string;
      view?: "structured" | "terminal";
    };
    expect(enableBody.session_id).toBe(sessionId);
    expect(enableBody.view === "structured").toBe(true);

    const sessionsAfterEnable = await listSessions(serve.baseUrl);
    expect(
      sessionsAfterEnable.find((s) => s.id === sessionId)!.view ===
        "structured",
    ).toBe(true);

    // Idempotent: a second enable returns the same shape without an
    // error and without re-spawning anything destructive.
    const enableAgain = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`,
      { method: "POST" },
    );
    expect(enableAgain.ok).toBeTruthy();
    const enableAgainBody = (await enableAgain.json()) as {
      view?: "structured" | "terminal";
    };
    expect(enableAgainBody.view === "structured").toBe(true);

    // structured view -> tmux
    const disableRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/disable`,
      { method: "POST" },
    );
    expect(disableRes.ok).toBeTruthy();
    const disableBody = (await disableRes.json()) as {
      session_id: string;
      view?: "structured" | "terminal";
    };
    expect(disableBody.view === "structured").toBe(false);

    const sessionsAfterDisable = await listSessions(serve.baseUrl);
    expect(
      sessionsAfterDisable.find((s) => s.id === sessionId)!.view ===
        "structured",
    ).toBe(false);

    // Idempotent in the other direction too.
    const disableAgain = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/disable`,
      { method: "POST" },
    );
    expect(disableAgain.ok).toBeTruthy();
    const disableAgainBody = (await disableAgain.json()) as {
      view?: "structured" | "terminal";
    };
    expect(disableAgainBody.view === "structured").toBe(false);
  } finally {
    await serve.stop();
  }
});
