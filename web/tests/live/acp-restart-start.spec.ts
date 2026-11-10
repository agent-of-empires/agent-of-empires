// Restarting a Structured view start that is stuck after the handshake, from the startup line.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { listSessions, waitForSessions } from "../helpers/aoeServe";
import { replayJson } from "../helpers/acp";

test("Restart on the startup line replaces a start stuck in session/new, again if the replacement stalls too", async ({
  page,
  spawnServe,
}) => {
  let marker = "";
  let debugLog = "";
  const serve = await spawnServe({
    acp: true,
    extraEnv: (home) => ({
      FAKE_ACP_STALL_NEW: (marker = join(home, "stall-new")),
      FAKE_ACP_STALL_NEW_TIMES: "2",
      FAKE_ACP_DEBUG_LOG: (debugLog = join(home, "fake-acp.log")),
    }),
  });
  const PROMPT = "typed during the stalled start";
  const stalls = () => (existsSync(marker) ? readFileSync(marker, "utf8").split("\n").length - 1 : 0);
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
  await expect.poll(stalls, { timeout: 15_000 }).toBe(1);
  const restart = line.getByRole("button", { name: "Restart Claude Code" });
  await expect(restart).toHaveCount(0);

  // The line says the composer is open; a prompt the daemon accepts now is carried across the restarts.
  const composer = page.locator('textarea[name="input"]');
  await composer.fill(PROMPT);
  // Stop is optimistic; the response proves the daemon took the prompt before Restart.
  const admitted = page.waitForResponse(
    (r) => r.url().endsWith("/acp/prompt") && r.request().postDataJSON()?.text === PROMPT,
  );
  await composer.press("Enter");
  const admission = await admitted;
  expect(admission.status(), await admission.text()).toBe(202);
  const dispatch = (await admission.json()) as { disposition?: string; queued_id?: string };
  expect(dispatch.disposition).toBe("sent");
  expect(dispatch.queued_id).toBeUndefined();
  const stopButton = page.getByRole("button", { name: "Stop", exact: true });
  await expect(stopButton).toBeVisible();

  await page.clock.fastForward(31_000);
  await expect(restart).toBeVisible();
  const restarted = page.waitForResponse((r) => r.url().endsWith("/acp/restart") && r.request().method() === "POST");
  await restart.click();
  const res = await restarted;
  expect(res.request().postDataJSON()).toEqual({});
  expect(res.status(), await res.text()).toBe(200);

  // The restart's own stop has reached this dashboard, and the replacement is stuck as well.
  await expect.poll(stalls, { timeout: 30_000 }).toBe(2);
  await expect(line).toBeVisible();
  await expect(stopButton).toBeVisible();
  await expect(page.getByText("Structured view worker stopped")).toHaveCount(0);
  await expect(restart).toHaveCount(0);
  await page.clock.fastForward(31_000);
  const again = page.waitForResponse((r) => r.url().endsWith("/acp/restart") && r.request().method() === "POST");
  await restart.click();
  expect((await again).status()).toBe(200);

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
  // The prompt reaches the replacement once, and the turn must finish before the log is read.
  await expect.poll(deliveries, { timeout: 15_000 }).toBe(1);
  await expect(stopButton).toHaveCount(0, { timeout: 15_000 });
  const readEvents = async () =>
    (JSON.parse(await replayJson(serve.baseUrl, session!.id)) as { event: Record<string, unknown> }[]).map(
      ({ event }) => event,
    );
  await expect
    .poll(
      async () =>
        (await readEvents()).some((e) => (e.Stopped as { reason?: string } | undefined)?.reason === "prompt_complete"),
      {
        timeout: 15_000,
      },
    )
    .toBe(true);
  // The event log is the check: the carried prompt is sent once and its turn is never stopped by a restart.
  const events = await readEvents();
  const sent = events.filter((e) => (e.UserPromptSent as { text?: string } | undefined)?.text === PROMPT);
  expect(sent).toHaveLength(1);
  const after = events.slice(events.indexOf(sent[0]!) + 1);
  expect(after.some((e) => "AcpSessionAssigned" in e)).toBe(true);
  const stopped = after.filter((e) => "Stopped" in e);
  expect(stopped).not.toContainEqual({ Stopped: { reason: "user_stopped" } });
  expect(stopped[0]).toEqual({ Stopped: { reason: "prompt_complete" } });
});
