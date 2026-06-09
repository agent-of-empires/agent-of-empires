// Browser-driven user stories for the structured view model + reasoning
// effort pickers (#1403). The sibling acp-config-pickers spec
// pins the HTTP / replay wire shape; this one drives the actual
// React surface: click the chip, pick a value, watch the chip
// reflect the adapter's confirming snapshot.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { waitForAcpReady } from "../helpers/acp";

async function enableAndWait(baseUrl: string, sessionId: string) {
  const enableRes = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
  expect(enableRes.ok).toBeTruthy();
  await waitForAcpReady(baseUrl, sessionId);
}

base("user sees model and effort pickers after the adapter advertises config options", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "ui-pickers-render" }),
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${sessionId}`);

    const modelChip = page.getByTestId("config-option-model");
    await expect(modelChip).toBeVisible({ timeout: 15_000 });
    await expect(modelChip).toContainText("Claude Opus 4.7");

    const effortControl = page.getByTestId("config-option-effort");
    await expect(effortControl).toBeVisible();
    await expect(effortControl).toContainText("Default");
    await expect(effortControl).toContainText("High");
  } finally {
    await serve.stop();
  }
});

base("user switches the model and the chip reflects the adapter confirmation", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "ui-pickers-switch-model" }),
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableAndWait(serve.baseUrl, sessionId);

    const postRequests: Array<{ url: string; body: string | null }> = [];
    page.on("request", (req) => {
      if (req.method() === "POST" && req.url().includes(`/acp/config-option`)) {
        postRequests.push({ url: req.url(), body: req.postData() });
      }
    });

    await page.goto(`${serve.baseUrl}/session/${sessionId}`);

    const modelChip = page.getByTestId("config-option-model");
    await expect(modelChip).toBeVisible({ timeout: 15_000 });
    await expect(modelChip).toContainText("Claude Opus 4.7");

    await modelChip.click();
    await page.getByTestId("config-option-model-value-claude-sonnet-4-6").click();

    // POST shape: { config_id: "model", value: "claude-sonnet-4-6" }.
    await expect.poll(() => postRequests.length).toBeGreaterThan(0);
    const body = postRequests[0]!.body;
    expect(body).not.toBeNull();
    const parsed = JSON.parse(body!);
    expect(parsed).toEqual({
      config_id: "model",
      value: "claude-sonnet-4-6",
    });

    // Adapter's confirming snapshot lands via WS; chip updates.
    await expect(modelChip).toContainText("Claude Sonnet 4.6", {
      timeout: 10_000,
    });
  } finally {
    await serve.stop();
  }
});

base("user picks reasoning effort and the segment becomes active", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "ui-pickers-switch-effort" }),
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${sessionId}`);

    const effortControl = page.getByTestId("config-option-effort");
    await expect(effortControl).toBeVisible({ timeout: 15_000 });

    const highSegment = page.getByTestId("config-option-effort-value-high");
    await highSegment.click();

    // After the adapter confirms, the High radio reports
    // aria-checked=true and Default no longer does.
    await expect(highSegment).toHaveAttribute("aria-checked", "true", {
      timeout: 10_000,
    });
    await expect(page.getByTestId("config-option-effort-value-default")).toHaveAttribute("aria-checked", "false");
  } finally {
    await serve.stop();
  }
});

base("rejected switch renders a dismissable non-blocking notice", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "ui-pickers-reject" }),
    extraEnv: { FAKE_ACP_REJECT_CONFIG_OPTION: "rate limited (test)" },
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${sessionId}`);

    const modelChip = page.getByTestId("config-option-model");
    await expect(modelChip).toBeVisible({ timeout: 15_000 });
    await modelChip.click();
    await page.getByTestId("config-option-model-value-claude-sonnet-4-6").click();

    const notice = page.getByTestId("config-option-switch-failed-notice");
    await expect(notice).toBeVisible({ timeout: 10_000 });
    await expect(notice).toContainText("rate limited (test)");

    // Chip stays on the previously-current value: pessimistic UI.
    await expect(modelChip).toContainText("Claude Opus 4.7");

    // Manual dismiss removes the notice.
    await notice.getByRole("button", { name: "Dismiss notice" }).click();
    await expect(notice).toHaveCount(0);
  } finally {
    await serve.stop();
  }
});
