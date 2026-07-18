// Structured view view switch (API level).
//
// The two endpoints (`POST /acp/enable` and
// `POST /acp/disable`) both return
// `{ session_id, view?: "structured" | "terminal" }` and persist the new view
// to the on-disk session record. This spec round-trips both directions
// and asserts the session-list reports the swap on each step.
//
// Independent of #1237: enable returns 200 even when the supervisor's
// async spawn later fails, and disable tears the worker down without
// going through the prompt path.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { waitForAcpReady } from "../helpers/acp";

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

    const spawnBeforeEnable = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/spawn`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    });
    expect(spawnBeforeEnable.status).toBe(409);
    const spawnBeforeEnableBody = (await spawnBeforeEnable.json()) as {
      error?: string;
    };
    expect(spawnBeforeEnableBody.error).toBe("not_structured");

    // tmux -> structured view
    const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableRes.ok).toBeTruthy();
    const enableBody = (await enableRes.json()) as {
      session_id: string;
      view?: "structured" | "terminal";
    };
    expect(enableBody.session_id).toBe(sessionId);
    expect(enableBody.view === "structured").toBe(true);

    // The enable response above is the authoritative synchronous ack. The
    // session list is a cache the daemon reconciles on a 2s tick, so a
    // disk snapshot taken just before the enable write can briefly clobber
    // the in-memory `view` back to terminal before self-correcting. Poll
    // rather than asserting the list immediately.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.view === "structured", {
        timeout: 10_000,
        intervals: [100, 200, 400],
      })
      .toBe(true);

    // Idempotent: a second enable returns the same shape without an
    // error and without re-spawning anything destructive.
    const enableAgain = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableAgain.ok).toBeTruthy();
    const enableAgainBody = (await enableAgain.json()) as {
      view?: "structured" | "terminal";
    };
    expect(enableAgainBody.view === "structured").toBe(true);

    // structured view -> tmux
    const disableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/disable`, { method: "POST" });
    expect(disableRes.ok).toBeTruthy();
    const disableBody = (await disableRes.json()) as {
      session_id: string;
      view?: "structured" | "terminal";
    };
    expect(disableBody.view === "structured").toBe(false);

    // Same 2s reconciler race as the enable direction; poll the list.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId)?.view === "structured", {
        timeout: 10_000,
        intervals: [100, 200, 400],
      })
      .toBe(false);

    // Idempotent in the other direction too.
    const disableAgain = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/disable`, { method: "POST" });
    expect(disableAgain.ok).toBeTruthy();
    const disableAgainBody = (await disableAgain.json()) as {
      view?: "structured" | "terminal";
    };
    expect(disableAgainBody.view === "structured").toBe(false);
  } finally {
    await serve.stop();
  }
});

// #2252: switching a claude structured session back to the terminal keeps the
// conversation. The captured ACP session id (claude SDK UUID) is carried into
// the terminal launch, which resumes it with `claude --resume <id>` instead of
// starting an empty pane. The fake agent shim logs its argv on every startup
// (fakeAcpAgent.mjs), so the terminal relaunch is observable in fake-acp.log.
test("switch to terminal resumes the claude conversation (keep context)", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-keep-context", tool: "claude" }),
  });

  try {
    const sessionId = (await listSessions(serve.baseUrl))[0]!.id;

    // tmux -> structured view, then wait for the ACP session/new handshake so
    // the daemon has captured an acp_session_id to carry over.
    const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableRes.ok).toBeTruthy();
    await waitForAcpReady(serve.baseUrl, sessionId, 30_000, serve.home);

    // acp_session_id is serialized on the session summary; poll until present.
    let acpSessionId = "";
    await expect
      .poll(
        async () => {
          const s = (await listSessions(serve.baseUrl)).find((s) => s.id === sessionId) as
            | { acp_session_id?: string }
            | undefined;
          acpSessionId = s?.acp_session_id ?? "";
          return acpSessionId;
        },
        { timeout: 15_000, intervals: [100, 200, 400] },
      )
      .not.toBe("");

    // structured view -> tmux with context preservation.
    const disableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/disable`, { method: "POST" });
    expect(disableRes.ok).toBeTruthy();
    expect(((await disableRes.json()) as { view?: string }).view === "structured").toBe(false);

    // The terminal pane relaunches the claude shim with `--resume <acp id>`.
    // The fake shim logs argv as a JSON array (fakeAcpAgent.mjs), so the flag
    // and its value are adjacent elements: `"--resume","<id>"`. Assert that
    // pair (not `--session-id <id>`, the fresh-start pin the destructive path
    // would have produced).
    const fakeLog = join(serve.home, "fake-acp.log");
    await expect
      .poll(() => (existsSync(fakeLog) ? readFileSync(fakeLog, "utf8") : ""), {
        timeout: 15_000,
        intervals: [100, 200, 400],
      })
      .toContain(`"--resume","${acpSessionId}"`);
  } finally {
    await serve.stop();
  }
});
