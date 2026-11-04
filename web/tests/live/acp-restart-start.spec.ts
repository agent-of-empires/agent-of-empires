// Restarting a Structured view start that is stuck after the handshake, from the startup line.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { listSessions, waitForSessions } from "../helpers/aoeServe";
import { replayJson } from "../helpers/acp";

test("Restart on the startup line replaces a start stuck in session/new", async ({ page, spawnServe }) => {
  let marker = "";
  let debugLog = "";
  const serve = await spawnServe({
    acp: true,
    extraEnv: (home) => ({
      FAKE_ACP_STALL_NEW_ONCE: (marker = join(home, "stall-new-once")),
      FAKE_ACP_DEBUG_LOG: (debugLog = join(home, "fake-acp.log")),
    }),
  });
  const PROMPT = "typed during the stalled start";
  const deliveries = () =>
    existsSync(debugLog)
      ? readFileSync(debugLog, "utf8")
          .split("\n")
          .filter((l) => l.includes("session/prompt") && l.includes(PROMPT)).length
      : 0;
  await page.clock.install();
  await page.goto(serve.baseUrl);
  await page.getByRole("button", { name: "New session", exact: true }).first().click();
  const wizard = page.getByTestId("session-wizard");
  await wizard.getByRole("button", { name: "Scratch", exact: true }).click();
  await wizard.getByRole("button", { name: "Use a scratch folder" }).click();
  await expect(wizard.getByRole("radio", { name: "Structured view" })).toBeChecked({ timeout: 10_000 });
  await wizard.getByRole("button", { name: /Launch session/ }).click();
  const [session] = await waitForSessions(serve.baseUrl);

  const line = page.getByTestId("startup-wait-line");
  await expect(line).toBeVisible({ timeout: 15_000 });
  await expect.poll(() => existsSync(marker), { timeout: 15_000 }).toBe(true);
  const restart = line.getByRole("button", { name: "Restart Claude Code" });
  await expect(restart).toHaveCount(0);

  // The line says the composer is open; a prompt the daemon accepts now is stopped by a restart.
  const composer = page.locator('textarea[name="input"]');
  await composer.fill(PROMPT);
  await composer.press("Enter");

  await page.clock.fastForward(31_000);
  await expect(line.getByTestId("startup-wait-restart-cost")).toHaveText(
    "Restarting stops the message you sent. Send it again once Claude Code is ready.",
  );
  const restarted = page.waitForResponse((r) => r.url().endsWith("/acp/restart") && r.request().method() === "POST");
  await restart.click();
  const res = await restarted;
  expect(res.request().postDataJSON()).toEqual({});
  expect(res.status(), await res.text()).toBe(200);

  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).find((s) => s.id === session!.id)?.acp_worker_state, {
      timeout: 30_000,
    })
    .toBe("running");
  await expect(line).toBeHidden({ timeout: 15_000 });
  // The cancelled start reports before the restarted one can run, so the log already holds any error it published.
  expect(await replayJson(serve.baseUrl, session!.id)).not.toContain("AgentStartupError");
  await expect(page.getByTestId("startup-error-banner")).toHaveCount(0);
  const status = (await listSessions(serve.baseUrl)).find((s) => s.id === session!.id)?.status;
  expect(status).not.toBe("error");
  await expect(page.getByText(PROMPT)).toBeVisible();
  await expect(composer).toBeEnabled();
  // The transcript's row for that empty turn doesn't carry the stop reason, so the event log is the check.
  const events = (JSON.parse(await replayJson(serve.baseUrl, session!.id)) as { event: Record<string, unknown> }[]).map(
    ({ event }) => event,
  );
  const sent = events.findIndex((e) => (e.UserPromptSent as { text?: string } | undefined)?.text === PROMPT);
  expect(sent).toBeGreaterThanOrEqual(0);
  const after = events.slice(sent + 1);
  const stop = after.findIndex((e) => "Stopped" in e);
  expect(after[stop]).toEqual({ Stopped: { reason: "user_stopped" } });
  // The replacement's initialize can log before the stop does; nothing else may.
  expect(after.slice(0, stop).every((e) => "AuthStatusUpdated" in e || "PromptCapabilities" in e)).toBe(true);
  expect(after.some((e) => "AcpSessionAssigned" in e)).toBe(true);
  expect(deliveries()).toBe(0);

  // The caption's advice: sending again reaches the replacement once.
  await composer.fill(PROMPT);
  await composer.press("Enter");
  await expect.poll(deliveries, { timeout: 15_000 }).toBe(1);
});
