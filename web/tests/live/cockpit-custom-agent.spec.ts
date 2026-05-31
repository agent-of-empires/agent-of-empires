// Live spec for #1579: a custom agent that declares an `agent_cockpit_cmd`
// can run in cockpit, both in the agent list the wizard reads and through
// the session-create round-trip.
//
// We seed a config.toml with two custom agents before the server boots:
//   - `oc-cockpit`   has an agent_cockpit_cmd  -> should be acp_capable
//   - `oc-terminal`  has none                  -> tmux-only
// then assert /api/agents reflects that, and that creating a cockpit
// session for each yields the right substrate (the server re-resolves
// capability and downgrades a non-capable agent to tmux instead of
// trusting the client or erroring at spawn time).

import { test as base, expect } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe, appDirFor, resolveAoeBinary } from "../helpers/aoeServe";

const CONFIG = `
[cockpit]
enabled = true

[session.custom_agents]
"oc-cockpit" = "true"
"oc-terminal" = "true"

[session.agent_cockpit_cmd]
"oc-cockpit" = "true acp"
`;

async function createSession(
  baseUrl: string,
  body: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const res = await fetch(`${baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  expect(res.ok, `POST /api/sessions failed: ${res.status} ${await res.clone().text()}`).toBeTruthy();
  const json = await res.json();
  // The handler returns the SessionResponse directly or wrapped in
  // `{ session }`; accept either.
  return (json.session ?? json) as Record<string, unknown>;
}

base("custom agent with agent_cockpit_cmd runs in cockpit", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, xdg }) => {
      const appDir = appDirFor(home, xdg, resolveAoeBinary());
      mkdirSync(appDir, { recursive: true });
      writeFileSync(join(appDir, "config.toml"), CONFIG);
    },
  });

  try {
    // Cockpit master on (config sets it, but PATCH is idempotent and
    // guards against the atomic not seeding from config at boot).
    await fetch(`${serve.baseUrl}/api/cockpit/master`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ enabled: true }),
    });

    // /api/agents reports acp_capable per the agent_cockpit_cmd config.
    const agentsRes = await fetch(`${serve.baseUrl}/api/agents`);
    expect(agentsRes.ok).toBeTruthy();
    const agents = (await agentsRes.json()) as Array<{
      name: string;
      kind: string;
      acp_capable: boolean;
    }>;
    const cockpitAgent = agents.find((a) => a.name === "oc-cockpit");
    const terminalAgent = agents.find((a) => a.name === "oc-terminal");
    expect(cockpitAgent, "oc-cockpit missing from /api/agents").toBeTruthy();
    expect(terminalAgent, "oc-terminal missing from /api/agents").toBeTruthy();
    expect(cockpitAgent!.acp_capable).toBe(true);
    expect(terminalAgent!.acp_capable).toBe(false);

    // Creating a cockpit session for the capable custom agent keeps
    // cockpit_mode on and reports acp_capable.
    const cockpitSession = await createSession(serve.baseUrl, {
      path: "",
      tool: "oc-cockpit",
      title: "cockpit-custom",
      cockpit_mode: true,
      scratch: true,
    });
    expect(cockpitSession.cockpit_mode).toBe(true);
    expect(cockpitSession.acp_capable).toBe(true);

    // The non-capable custom agent is downgraded to tmux by the server
    // even though the client asked for cockpit.
    const terminalSession = await createSession(serve.baseUrl, {
      path: "",
      tool: "oc-terminal",
      title: "terminal-custom",
      cockpit_mode: true,
      scratch: true,
    });
    expect(terminalSession.cockpit_mode).toBe(false);
    expect(terminalSession.acp_capable).toBe(false);
  } finally {
    await serve.stop();
  }
});
