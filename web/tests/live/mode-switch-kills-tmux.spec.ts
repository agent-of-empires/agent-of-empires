// Structured-view mode-switch full tmux teardown (#1869, validating the #1867
// wiring at the `acp_enable` handler level).
//
// Switching a tmux-mode session to structured view (`POST /acp/enable`) must
// reap EVERY tmux session the instance owns, not just the agent pane: the web
// terminal panel(s) and any tool sub-sessions go too, via
// `Instance::kill_ancillary_tmux_sessions`. History is destroyed in the swap,
// so a leaked terminal/tool pane would orphan a live shell the user can no
// longer reach.
//
// The ancillary reapers (`kill_all_terminals_for_id` /
// `kill_all_tool_sessions_for_id`) find their targets by scanning
// `tmux list-sessions` for the session-id suffix, so pre-creating real tmux
// sessions under the aoe naming convention is exactly what the handler sees at
// runtime from a live terminal panel + tool session. We stand up all three
// kinds on the daemon's isolated socket, flip to structured view, and assert
// every one is gone.

import { test, expect } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd, waitForView } from "../helpers/aoeServe";

// aoe (debug build) routes tmux through an explicit `-S <socket>` and ignores
// TMUX_TMPDIR (#2608), so specs shelling out to a raw `tmux` MUST target the
// same socket the daemon uses (`serve.tmuxSocket`).
function tmuxNewDetached(socket: string, name: string): boolean {
  const res = spawnSync("tmux", [
    "-S",
    socket,
    "new-session",
    "-d",
    "-s",
    name,
    "-x",
    "80",
    "-y",
    "24",
    "sleep",
    "600",
  ]);
  return res.status === 0;
}

function tmuxHasSession(socket: string, name: string): boolean {
  const res = spawnSync("tmux", ["-S", socket, "has-session", "-t", name]);
  return res.status === 0;
}

function tmuxKill(socket: string, name: string): void {
  spawnSync("tmux", ["-S", socket, "kill-session", "-t", name]);
}

test("enabling structured view reaps agent, terminal, and tool tmux sessions", async ({}, testInfo) => {
  const title = "modeswitch";
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title, tool: "claude" }),
  });

  // Names are derived the same way the running binary composes them:
  //   agent    = <prefix><title>_<id8>
  //   terminal = <prefix>term_<title>_<id8>   (host terminal, index 0)
  //   tool     = <prefix>tool_<tool>_<title>_<id8>
  // <prefix> is aoe_ / aoe_dev_ (serve.tmuxPrefix); TERMINAL_PREFIX and
  // TOOL_PREFIX are that prefix plus `term_` / `tool_`.
  let agentName = "";
  let terminalName = "";
  let toolName = "";

  try {
    const sessionsBefore = await listSessions(serve.baseUrl);
    const sessionId = sessionsBefore[0]!.id;
    // `aoe add` defaults to tmux mode.
    expect(sessionsBefore[0]!.view === "structured").toBeFalsy();

    const id8 = sessionId.slice(0, 8);
    agentName = `${serve.tmuxPrefix}${title}_${id8}`;
    terminalName = `${serve.tmuxPrefix}term_${title}_${id8}`;
    toolName = `${serve.tmuxPrefix}tool_lazygit_${title}_${id8}`;

    // Stand up the three owned tmux sessions the switch must reap.
    expect(tmuxNewDetached(serve.tmuxSocket, agentName)).toBe(true);
    expect(tmuxNewDetached(serve.tmuxSocket, terminalName)).toBe(true);
    expect(tmuxNewDetached(serve.tmuxSocket, toolName)).toBe(true);
    expect(tmuxHasSession(serve.tmuxSocket, agentName)).toBe(true);
    expect(tmuxHasSession(serve.tmuxSocket, terminalName)).toBe(true);
    expect(tmuxHasSession(serve.tmuxSocket, toolName)).toBe(true);

    // tmux -> structured view. The synchronous response is authoritative.
    const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableRes.ok).toBeTruthy();
    const enableBody = (await enableRes.json()) as {
      session_id: string;
      view?: "structured" | "terminal";
    };
    expect(enableBody.session_id).toBe(sessionId);
    expect(enableBody.view === "structured").toBe(true);

    // The teardown runs on a blocking pool worker before the handler returns,
    // but the reaper's SIGTERM grace + tmux settle can lag the response; poll.
    await expect
      .poll(() => tmuxHasSession(serve.tmuxSocket, agentName), { timeout: 10_000, intervals: [100, 200, 400] })
      .toBe(false);
    await expect
      .poll(() => tmuxHasSession(serve.tmuxSocket, terminalName), { timeout: 10_000, intervals: [100, 200, 400] })
      .toBe(false);
    await expect
      .poll(() => tmuxHasSession(serve.tmuxSocket, toolName), { timeout: 10_000, intervals: [100, 200, 400] })
      .toBe(false);

    // The view genuinely converged (not just a teardown side effect).
    await waitForView(serve.baseUrl, sessionId, "structured");
  } finally {
    // Best-effort: reap anything that survived a mid-test failure.
    if (agentName) tmuxKill(serve.tmuxSocket, agentName);
    if (terminalName) tmuxKill(serve.tmuxSocket, terminalName);
    if (toolName) tmuxKill(serve.tmuxSocket, toolName);
    await serve.stop();
  }
});
