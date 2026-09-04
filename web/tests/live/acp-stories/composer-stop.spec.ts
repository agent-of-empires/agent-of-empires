// User story: the Stop button cancels the running turn, whatever the agent
// happens to be doing when it is pressed.
//
// Clicking Stop dispatches `runtime.cancelRun()`, which POSTs /acp/cancel;
// the fake responds with stopped { cancelled } and the composer flips back to
// the idle "Send a message…" placeholder.
//
// One spec, four turns, because the four cases only ever differed in the
// single `session/update` the fake emits before an identical `wait_ms` hold:
// a message chunk, a thought chunk, a pending tool call, and a sub-agent Task
// with a child tool call. Everything after the click was byte-identical, so
// they always failed together, and four copies meant four `aoe serve` boots,
// four agent subprocesses and four browser contexts to vary one field. The
// sub-agent case is the one worth naming: child tool calls carry
// `_meta.claudeCode.parentToolUseId` and render grouped under a parent, so a
// refactor of that grouping must not take the parent Stop path with it.
//
// The post-hold chunk in case 1 is deliberately NOT asserted absent: the fake
// is single-threaded JS and does not abort its in-flight session/prompt loop
// when session/cancel arrives, so it may still land after Stop. The server's
// cancel semantics belong to the REST-level acp-cancel spec.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait, attachServeDiagnostics } from "../../helpers/acp";

// 30s holds each turn open longer than the assertions below will ever wait,
// so the Stop affordance stays mounted even on a heavily loaded runner where
// the first update and the click can be tens of seconds apart.
const HOLD = { sessionUpdate: "wait_ms", ms: 30_000 };

const CASES = [
  {
    name: "streaming a message",
    prompt: "start a long turn",
    updates: [
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Thinking..." } },
      HOLD,
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Should never appear." } },
    ],
    // The only case with a distinct pre-Stop signal worth asserting; the
    // others surface as the Stop affordance itself.
    marker: "Thinking...",
  },
  {
    name: "reasoning",
    prompt: "think about this",
    updates: [
      { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Reasoning about the problem..." } },
      HOLD,
    ],
    marker: null,
  },
  {
    name: "running a tool",
    prompt: "run a slow tool",
    updates: [
      { sessionUpdate: "tool_call", toolCallId: "tc-stop-tool-1", title: "Slow tool", kind: "read", status: "pending" },
      HOLD,
    ],
    marker: "Slow tool",
  },
  {
    name: "running a sub-agent task",
    prompt: "investigate this",
    updates: [
      {
        sessionUpdate: "tool_call",
        toolCallId: "parent-task",
        title: "Task: investigate",
        kind: "task",
        status: "pending",
      },
      {
        sessionUpdate: "tool_call",
        toolCallId: "child-read",
        title: "Read file",
        kind: "read",
        status: "pending",
        _meta: { claudeCode: { parentToolUseId: "parent-task" } },
      },
      HOLD,
    ],
    marker: "Task: investigate",
  },
] as const;

const SCRIPT = {
  turns: CASES.map((c) => ({ updates: c.updates, stopReason: "end_turn" })),
};

base("Stop cancels the turn whatever the agent is doing", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-stop-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-stop" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-stop");
    if (!seeded) throw new Error("seeded session 'story-stop' missing");

    await enableStructuredViewAndWait(serve.baseUrl, seeded.id);
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded.id)}`);
    await waitForStructuredView(page);

    // Scoped to the composer: `name` matches by substring, so a bare
    // `{ name: "Stop" }` also picks up the queued strip's "Stop the current
    // turn and send this message now" button, and the two matches fail
    // Playwright's strict mode.
    const stopButton = page.getByTestId("composer-actions").getByRole("button", { name: "Stop" });
    const idleComposer = page.getByRole("textbox", { name: /Send a message/i });

    for (const c of CASES) {
      await base.step(`Stop while ${c.name}`, async () => {
        await idleComposer.fill(c.prompt);
        await idleComposer.press("Enter");
        if (c.marker) {
          await expect(page.getByText(c.marker).first()).toBeVisible({ timeout: 15_000 });
        }
        await expect(stopButton).toBeVisible({ timeout: 15_000 });
        await stopButton.click();
        // The turn ended: the composer is editable again and Stop is gone.
        await expect(idleComposer).toBeVisible({ timeout: 15_000 });
        await expect(stopButton).toBeHidden({ timeout: 15_000 });
      });
    }
  } finally {
    try {
      if (serveHandle) await attachServeDiagnostics(testInfo, serveHandle);
    } catch {
      // best-effort diagnostics; do not block cleanup
    }
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
