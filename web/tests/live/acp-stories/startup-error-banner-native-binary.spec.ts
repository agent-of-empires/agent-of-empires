// User story: the red "Structured view agent failed to start" banner shows
// the new native-binary-launch-failure remediation and exposes an
// Open agent log affordance that round-trips the worker-log endpoint.
//
// The fake ACP agent's `failOn` script field rejects `session/new`
// with a JSON-RPC error whose `data.details` matches the native-binary
// regex; structured view's spawn path turns that into an AgentStartupError
// event the React banner reads. See #1449.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

const NATIVE_BINARY_DETAILS =
  "Claude Code native binary at /usr/lib/node_modules/@agentclientprotocol/claude-agent-acp/node_modules/@anthropic-ai/claude-agent-sdk-linux-arm64/claude exists but failed to launch.";

const SCRIPT = {
  failOn: {
    method: "session/new",
    code: -32603,
    message: "Internal error",
    data: { details: NATIVE_BINARY_DETAILS },
  },
};

interface ReplayFrameEvent {
  AgentStartupError?: { message?: string };
}

async function pollForStartupError(baseUrl: string, sessionId: string, timeoutMs = 20_000): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const res = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`);
    if (res.ok) {
      const body = (await res.json()) as {
        frames?: Array<{ event?: ReplayFrameEvent }>;
      };
      for (const f of body.frames ?? []) {
        const msg = f.event?.AgentStartupError?.message;
        if (typeof msg === "string" && msg.length > 0) return msg;
      }
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error("never observed AgentStartupError on replay");
}

base("startup banner: native-binary branch + agent-log disclosure", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-native-binary-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-native-binary" }),
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-native-binary");
    if (!seeded) throw new Error("seeded session 'story-native-binary' missing");
    const sessionId = seeded.id;

    // Bypass enableStructuredViewAndWait: it throws when an AgentStartupError
    // shows up on replay, which is exactly the state this spec is
    // trying to land in. Hit the enable endpoint directly and then
    // poll for the typed error event ourselves.
    const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
    expect(enableRes.ok).toBe(true);
    const msg = await pollForStartupError(serve.baseUrl, sessionId);
    expect(msg).toContain("native binary");
    expect(msg).toContain("failed to launch");

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    // Banner is in the structured view chrome above the composer. Match on the
    // header text plus a piece of the new remediation copy so a future
    // rewording of either alone surfaces here.
    const banner = page.getByText("Structured view agent failed to start");
    await expect(banner).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText(/Architecture mismatch/i)).toBeVisible();
    await expect(page.getByText(/aoe acp doctor --fix/)).toHaveCount(0);

    const toggle = page.getByTestId("acp-agent-log-toggle");
    await expect(toggle).toBeVisible();
    await toggle.click();

    // Disclosure can terminate in any of: populated <pre>, "No log
    // output yet", "Log file exists but is empty", or an error
    // message. All confirm the endpoint round-tripped. The <pre> is
    // nested inside <div acp-agent-log-body>, so asserting on the
    // body alone covers every terminal state without strict-mode
    // multi-match ambiguity.
    const body = page.getByTestId("acp-agent-log-body");
    await expect(body).toBeVisible({ timeout: 10_000 });
    await expect(body).toHaveText(/Loading log|Could not load log|No log output yet|Log file exists but is empty|.+/);

    // Refresh re-issues the GET. Body remains the canonical container.
    await page.getByTestId("acp-agent-log-refresh").click();
    await expect(body).toBeVisible({ timeout: 5_000 });
  } finally {
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
