// #1403: the structured-view model and reasoning-effort pickers, replaying
// canned ConfigOptionsUpdated frames. The UI is pessimistic, so the chip only
// moves once the confirming frame lands and each test's `onConfigOption` plays
// the adapter's confirmation or rejection. The live spec pins the wire shape.

import { test, expect, publishedRequests } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession, configOptionsUpdated } from "./helpers/acpMock";

function modelOption(current: string) {
  return {
    id: "model",
    name: "Model",
    category: "model",
    current_value: current,
    options: [
      { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
      { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
    ],
  };
}

function longModelOption(current: string, count: number) {
  return {
    id: "model",
    name: "Model",
    category: "model",
    current_value: current,
    options: Array.from({ length: count }, (_, i) => ({
      value: `model-${i}`,
      name: `Model ${i}`,
    })),
  };
}

function effortOption(current: string) {
  return {
    id: "effort",
    name: "Reasoning Effort",
    category: "thought_level",
    current_value: current,
    options: [
      { value: "default", name: "Default" },
      { value: "low", name: "Low" },
      { value: "medium", name: "Medium" },
      { value: "high", name: "High" },
    ],
  };
}

function snapshot(model: string, effort: string) {
  return configOptionsUpdated([modelOption(model), effortOption(effort)]);
}

test("advertised pickers render; switching the model posts it and the chip follows the adapter confirmation", async ({
  page,
}) => {
  const mock = await mockAcpSession(page, {
    title: "ui-pickers-switch-model",
    initialEvents: [snapshot("claude-opus-4-7", "default")],
    // The adapter accepts the switch and resends the full snapshot with
    // the new current value.
    onConfigOption: (body) => [snapshot(body.value, "default")],
  });
  await openStructuredSession(page, mock);

  const modelChip = page.getByTestId("config-option-model");
  await expect(modelChip).toBeVisible({ timeout: 15_000 });
  await expect(modelChip).toContainText("Claude Opus 4.7");
  const effortControl = page.getByTestId("config-option-effort");
  await expect(effortControl).toContainText("Default");
  await expect(effortControl).toContainText("High");

  await modelChip.click();
  await page.getByTestId("config-option-model-value-claude-sonnet-4-6").click();

  // POST shape: { config_id: "model", value: "claude-sonnet-4-6" }.
  await expect.poll(() => mock.configOptionBodies.length).toBeGreaterThan(0);
  expect(mock.configOptionBodies[0]).toEqual({
    config_id: "model",
    value: "claude-sonnet-4-6",
  });

  // Adapter's confirming snapshot lands via WS; chip updates.
  await expect(modelChip).toContainText("Claude Sonnet 4.6", {
    timeout: 10_000,
  });
});

test("model search focuses on open, handles keyboard selection, and resets after dismissal", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "ui-pickers-model-search",
    initialEvents: [snapshot("claude-opus-4-7", "default")],
    onConfigOption: (body) => [snapshot(body.value, "default")],
  });
  await openStructuredSession(page, mock);

  const modelChip = page.getByTestId("config-option-model");
  await expect(modelChip).toBeVisible({ timeout: 15_000 });
  await modelChip.click();
  const search = page.getByRole("textbox", { name: "Search models" });
  await expect(search).toBeFocused();
  await search.fill("no-such-model");
  await expect(page.getByRole("menu", { name: "Model" }).getByRole("status")).toHaveText(
    "No models match your search.",
  );
  await search.press("ArrowDown");
  await expect(search).toBeFocused();
  await search.press("Enter");
  await expect(search).toBeVisible();
  await search.fill("CLAUDE 4-6");
  const sonnet = page.getByTestId("config-option-model-value-claude-sonnet-4-6");
  await expect(page.getByRole("menuitem")).toHaveCount(1);
  await expect(sonnet).toBeVisible();
  await search.press("Escape");
  await expect(search).toBeHidden();
  await expect(modelChip).toBeFocused();
  await expect(modelChip).toContainText("Claude Opus 4.7");

  await modelChip.click();
  await expect(search).toHaveValue("");
  await expect(page.getByRole("menuitem")).toHaveCount(2);
  const opus = page.getByTestId("config-option-model-value-claude-opus-4-7");
  await search.press("ArrowUp");
  await expect(sonnet).toBeFocused();
  await sonnet.press("ArrowDown");
  await expect(opus).toBeFocused();
  await opus.press("ArrowDown");
  await expect(sonnet).toBeFocused();
  await sonnet.press("Enter");
  await expect(modelChip).toContainText("Claude Sonnet 4.6");
  expect(mock.configOptionBodies).toEqual([{ config_id: "model", value: "claude-sonnet-4-6" }]);
  expect(await publishedRequests(page, `/api/sessions/${mock.sessionId}/acp/prompt`, "POST")).toEqual([]);

  await modelChip.click();
  await expect(search).toBeFocused();
  await search.fill("opus");
  await search.press("Enter");
  await expect(modelChip).toContainText("Claude Opus 4.7");
  expect(mock.configOptionBodies).toEqual([
    { config_id: "model", value: "claude-sonnet-4-6" },
    { config_id: "model", value: "claude-opus-4-7" },
  ]);

  await modelChip.click();
  await search.fill("sonnet");
  await page.getByRole("textbox", { name: /Send a message/i }).click();
  await expect(search).toBeHidden();
  await modelChip.click();
  await expect(search).toHaveValue("");
  await expect(page.getByRole("menuitem")).toHaveCount(2);
});

for (const height of [800, 320]) {
  test(`model menu opens at the current model and keeps search visible at ${height}px height`, async ({ page }) => {
    await page.setViewportSize({ width: 1280, height });
    const mock = await mockAcpSession(page, {
      title: "ui-pickers-current-model",
      initialEvents: [configOptionsUpdated([longModelOption("model-28", 40)])],
      onConfigOption: (body) => [configOptionsUpdated([longModelOption(body.value, 40)])],
    });
    await openStructuredSession(page, mock);

    const modelChip = page.getByTestId("config-option-model");
    await expect(modelChip).toBeVisible({ timeout: 15_000 });
    const triggerBox = await modelChip.boundingBox();
    await modelChip.click();

    const menu = page.locator('[id^="config-option-menu-model"]');
    await expect(menu).toBeVisible();

    const viewportSize = page.viewportSize();
    expect(viewportSize).not.toBeNull();
    const menuBox = await menu.boundingBox();
    expect(menuBox).not.toBeNull();
    expect(menuBox!.y).toBeGreaterThanOrEqual(0);
    expect(menuBox!.y + menuBox!.height).toBeLessThanOrEqual(viewportSize!.height);

    const scrollContainer = menu.locator(".overflow-y-auto");
    const { scrollHeight, clientHeight } = await scrollContainer.evaluate((el) => ({
      scrollHeight: el.scrollHeight,
      clientHeight: el.clientHeight,
    }));
    expect(scrollHeight).toBeGreaterThan(clientHeight);
    const search = page.getByRole("textbox", { name: "Search models" });
    const current = page.getByTestId("config-option-model-value-model-28");
    await expect(current).toBeInViewport({ ratio: 1 });
    await expect(current).toContainText("Active");
    const currentBox = (await current.boundingBox())!;
    const listBox = (await scrollContainer.boundingBox())!;
    expect(Math.abs(currentBox.y + currentBox.height / 2 - (listBox.y + listBox.height / 2))).toBeLessThan(1);
    expect(await modelChip.boundingBox()).toEqual(triggerBox);
    await expect(search).toBeFocused();
    await search.press("Enter");
    await expect(search).toBeHidden();
    expect(mock.configOptionBodies).toEqual([]);
    await modelChip.click();
    await expect(current).toBeInViewport({ ratio: 1 });

    await search.fill("Model 1");
    await expect(page.getByTestId("config-option-model-value-model-1")).toBeInViewport({ ratio: 1 });
    expect(await scrollContainer.evaluate((el) => el.scrollTop)).toBe(0);
    await search.fill("");
    await expect(current).toBeInViewport({ ratio: 1 });
    await expect(search).toBeFocused();

    await scrollContainer.evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect(current).not.toBeInViewport();
    await search.press("Escape");
    await modelChip.click();
    await expect(current).toBeInViewport({ ratio: 1 });
    await expect(search).toBeFocused();
    await search.press("ArrowUp");
    const previous = page.getByTestId("config-option-model-value-model-27");
    await expect(previous).toBeFocused();
    await previous.press("ArrowDown");
    await expect(current).toBeFocused();
    await search.fill("Model 39");
    await search.press("ArrowDown");
    const last = page.getByTestId("config-option-model-value-model-39");
    await expect(last).toBeFocused();
    await expect(search).toBeInViewport();
    await expect(last).toBeInViewport();
    await last.press("Enter");
    await expect(modelChip).toContainText("Model 39");
    await modelChip.click();
    await expect(last).toBeInViewport({ ratio: 0.98 });
    await expect(last).toContainText("Active");
    await expect(search).toBeFocused();
  });
}
