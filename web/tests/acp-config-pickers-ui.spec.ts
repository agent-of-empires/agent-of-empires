// #1403: the structured-view model and reasoning-effort pickers, replaying
// canned ConfigOptionsUpdated frames. The UI is pessimistic, so the chip only
// moves once the confirming frame lands and each test's `onConfigOption` plays
// the adapter's confirmation or rejection. The live spec pins the wire shape.

import { test, expect } from "./helpers/mockedTest";
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

test("model menu stays on-screen and scrollable on a short viewport", async ({ page }) => {
  // A long option list plus a short viewport is exactly the geometry the
  // fixed max-height missed: the menu opens upward from the composer
  // footer, so its true ceiling is the trigger's distance from the top of
  // the viewport, not a flat guess (review on #3747).
  await page.setViewportSize({ width: 800, height: 320 });
  const mock = await mockAcpSession(page, {
    title: "ui-pickers-short-viewport",
    initialEvents: [configOptionsUpdated([longModelOption("model-0", 40)])],
  });
  await openStructuredSession(page, mock);

  const modelChip = page.getByTestId("config-option-model");
  await expect(modelChip).toBeVisible({ timeout: 15_000 });
  await modelChip.click();

  const menu = page.locator('[id^="config-option-menu-model"]');
  await expect(menu).toBeVisible();

  const viewportSize = page.viewportSize();
  expect(viewportSize).not.toBeNull();
  const menuBox = await menu.boundingBox();
  expect(menuBox).not.toBeNull();
  expect(menuBox!.y).toBeGreaterThanOrEqual(0);
  expect(menuBox!.y + menuBox!.height).toBeLessThanOrEqual(viewportSize!.height);

  // The capped menu must actually scroll, not just fit on-screen by
  // silently truncating the option list: the scrollable list's content
  // height must exceed what's visible.
  const scrollContainer = menu.locator(".overflow-y-auto");
  const { scrollHeight, clientHeight } = await scrollContainer.evaluate((el) => ({
    scrollHeight: el.scrollHeight,
    clientHeight: el.clientHeight,
  }));
  expect(scrollHeight).toBeGreaterThan(clientHeight);
});
