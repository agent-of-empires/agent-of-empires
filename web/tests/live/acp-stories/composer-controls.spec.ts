// Structured view composer controls: slash command picks, Stop, and the mode picker.

import type { Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import {
  HOLD,
  chunk,
  endTurn,
  idleComposer,
  openStructuredView,
  replayFrames,
  script,
  spawnAcpAgent,
  startAcpSession,
  stopButton,
  waitForReplayContains,
  waitForStructuredView,
} from "../../helpers/acp";

const REVIEW_COMMAND = { name: "review", description: "Review the diff", accepts_input: true, hint: "what to review" };

// #1512: without the trailing space the popover reopens and claims the next Enter. The args-command
// variant (trailing space, caret after it) is pinned by Composer.test.tsx.
test("picking a no-arg slash command does not trap Enter", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-slash-pick-no-arg",
    extraEnv: {
      FAKE_ACP_COMMANDS: JSON.stringify([
        { name: "help", description: "Show help", accepts_input: false },
        REVIEW_COMMAND,
      ]),
    },
  });
  // An explicit spawn attaches the session before the first available_commands_update.
  await spawnAcpAgent(serve.baseUrl, sessionId);
  await waitForReplayContains(serve.baseUrl, sessionId, "AvailableCommandsUpdated");

  const composer = await openStructuredView(page, serve, sessionId);
  await composer.click();
  await composer.pressSequentially("/h");
  const item = page.getByRole("option").filter({ hasText: /\/help/ });
  await expect(item).toBeVisible({ timeout: 15_000 });
  await composer.press("Enter");

  await expect(composer).toHaveValue("/help ", { timeout: 5_000 });
  await expect(item).toBeHidden({ timeout: 5_000 });
  await composer.press("Enter");
  await expect(page.getByText("Hello from fake ACP agent.")).toBeVisible({ timeout: 10_000 });
  await expect(composer).toHaveValue("", { timeout: 5_000 });
});

const STOP_CASES = [
  {
    name: "streaming a message",
    prompt: "start a long turn",
    updates: [chunk("Thinking..."), HOLD, chunk("Should never appear.")],
    marker: "Thinking...",
  },
  {
    name: "reasoning",
    prompt: "think about this",
    updates: [
      { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Reasoning about the problem..." } },
      HOLD,
    ],
    marker: "ThinkingStarted",
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
    // Child tool calls render grouped under their parent; the grouping must keep the Stop path.
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
];

test("Stop cancels the turn whatever the agent is doing", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-stop",
    fakeAcpScript: script(...STOP_CASES.map((c) => endTurn(...c.updates))),
  });
  await openStructuredView(page, serve, sessionId);
  const replay = async () => JSON.stringify(await replayFrames(serve.baseUrl, sessionId));
  const cancelledTurns = async () => ((await replay()).match(/"reason":"cancelled"/g) ?? []).length;
  const idle = idleComposer(page);

  for (const c of STOP_CASES) {
    await test.step(`Stop while ${c.name}`, async () => {
      const cancelledBefore = await cancelledTurns();
      await idle.fill(c.prompt);
      await idle.press("Enter");
      await expect.poll(replay).toContain(c.marker);
      await expect(stopButton(page)).toBeVisible({ timeout: 15_000 });
      await stopButton(page).click();
      await expect(idle).toBeVisible({ timeout: 15_000 });
      await expect(stopButton(page)).toBeHidden({ timeout: 15_000 });
      await expect.poll(cancelledTurns).toBe(cancelledBefore + 1);
      await expect(page.getByText("Should never appear.")).toHaveCount(0);
    });
  }
});

test("stopping mid-tool settles the card and survives reload", async ({ page, spawnServe }) => {
  // #1646: a cancel emits no per-tool completion, so the card used to stay "running" forever.
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-stuck-tool",
    fakeAcpScript: script(endTurn(STOP_CASES[2]!.updates[0]!, HOLD)),
  });
  await openStructuredView(page, serve, sessionId, "run a slow tool");
  // The exact title excludes the prompt echo and the "Operating Slow tool…" spinner.
  const card = () =>
    page.getByText("Slow tool", { exact: true }).locator("xpath=ancestor::div[contains(@class,'rounded-md')][1]");
  await expect(card()).toBeVisible({ timeout: 10_000 });
  await expect(card().getByText("running", { exact: true })).toBeVisible({ timeout: 10_000 });

  await expect(stopButton(page)).toBeVisible({ timeout: 10_000 });
  await stopButton(page).click();

  const expectStopped = async () => {
    await expect(card()).toBeVisible({ timeout: 10_000 });
    await expect(card().getByText("stopped", { exact: true })).toBeVisible({ timeout: 10_000 });
    await expect(card().getByText("running", { exact: true })).toBeHidden();
  };
  await expectStopped();
  await page.reload();
  await waitForStructuredView(page);
  await expectStopped();
});

function modeTrigger(page: Page, labels: RegExp) {
  return page
    .locator("button")
    .filter({ has: page.locator(":scope > span", { hasText: labels }) })
    .first();
}

async function pickMode(page: Page, mode: RegExp) {
  const item = page.locator('[role="menu"]').getByText(mode).first();
  await expect(item).toBeVisible({ timeout: 5_000 });
  await item.click();
}

test("ModePicker uses OpenCode's config-option modes and never traps the user", async ({ page, spawnServe }) => {
  // #1764: OpenCode advertises modes only as a config option and rejects claude's phantom "Default".
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "story-mode-opencode",
    tool: "opencode",
    extraEnv: { FAKE_ACP_MODE_VIA_CONFIG_OPTION: "1" },
  });
  await spawnAcpAgent(serve.baseUrl, sessionId, "opencode");
  await openStructuredView(page, serve, sessionId);

  const trigger = modeTrigger(page, /^(Build|Plan)$/);
  await expect(trigger).toBeVisible({ timeout: 10_000 });
  // Scoped to the chip: the reasoning-effort selector has its own "Default".
  await expect(trigger).toContainText(/Build/i);

  await trigger.click();
  await expect(page.locator('[role="menu"]').getByText(/^Default$/)).toHaveCount(0);
  await pickMode(page, /^Plan$/i);
  await expect(trigger).toContainText(/Plan/i, { timeout: 10_000 });

  // Switching back used to fail with "mode not found".
  await trigger.click();
  await pickMode(page, /^Build$/i);
  await expect(trigger).toContainText(/Build/i, { timeout: 10_000 });
});
