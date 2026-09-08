// Mocked port of the live acp-config-pickers-ui spec: browser-driven
// user stories for the structured view model + reasoning effort pickers
// (#1403), replaying canned ConfigOptionsUpdated frames instead of a
// real daemon. The live acp-config-pickers spec (KEPT) pins the HTTP /
// replay wire shape against the real backend; this one drives the
// actual React surface: click the chip, pick a value, watch the chip
// reflect the adapter's confirming snapshot.
//
// The UI is pessimistic: the chip only moves once the confirming
// `ConfigOptionsUpdated` frame lands, so each test's `onConfigOption`
// handler plays the adapter's confirmation (or rejection).

import { test, expect } from "./helpers/mockedTest";
import {
  mockAcpSession,
  openStructuredSession,
  configOptionsUpdated,
  configOptionSwitchFailed,
} from "./helpers/acpMock";

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

test("user sees model and effort pickers after the adapter advertises config options", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "ui-pickers-render",
    initialEvents: [snapshot("claude-opus-4-7", "default")],
  });
  await openStructuredSession(page, mock);

  const modelChip = page.getByTestId("config-option-model");
  await expect(modelChip).toBeVisible({ timeout: 15_000 });
  await expect(modelChip).toContainText("Claude Opus 4.7");

  const effortControl = page.getByTestId("config-option-effort");
  await expect(effortControl).toBeVisible();
  await expect(effortControl).toContainText("Default");
  await expect(effortControl).toContainText("High");
});

test("user switches the model and the chip reflects the adapter confirmation", async ({ page }) => {
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

test("user picks reasoning effort and the segment becomes active", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "ui-pickers-switch-effort",
    initialEvents: [snapshot("claude-opus-4-7", "default")],
    onConfigOption: (body) => [snapshot("claude-opus-4-7", body.value)],
  });
  await openStructuredSession(page, mock);

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

test("rejected switch renders a dismissable non-blocking notice", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: "ui-pickers-reject",
    initialEvents: [snapshot("claude-opus-4-7", "default")],
    // The adapter rejects the switch; the daemon broadcasts the failure
    // frame instead of a confirming snapshot.
    onConfigOption: (body) => [configOptionSwitchFailed(body.config_id, body.value, "rate limited (test)")],
  });
  await openStructuredSession(page, mock);

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
});
