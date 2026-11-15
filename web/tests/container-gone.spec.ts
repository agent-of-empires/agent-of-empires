// The startup error banner for a sandboxed Session whose container is gone (#5652).

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";
import { wizard } from "./helpers/wizard";

const TITLE = "story-container-gone";
const MESSAGE = "This Session's sandbox container was removed. Start the Session again to create a new one.";

async function openGoneSession(page: Page, { readOnly = false, queued = false } = {}) {
  const mock = await mockAcpSession(page, {
    title: TITLE,
    about: readOnly ? { read_only: true } : {},
    workerState: "absent",
    acpSessionId: "acp-1",
    sessionFields: { is_sandboxed: true, group_path: "infra", acp_agent: "claude" },
    initialEvents: [{ AgentStartupError: { message: MESSAGE, cause: { kind: "container_missing" } } }],
  });
  const recreates: string[] = [];
  await page.route("**/api/sessions/*/recreate-container", (r) => {
    recreates.push(r.request().method());
    return r.fulfill({ json: {} });
  });
  await page.route("**/api/sessions/*/acp/install-info", (r) =>
    r.fulfill({ json: { agent: "claude", display_name: "Claude", sandboxed: true } }),
  );
  if (queued) {
    const at = new Date().toISOString();
    await page.route("**/api/sessions/*/queue", (r) =>
      r.fulfill({ json: [{ id: "q1", seq: 1, text: "first", created_at: at }] }),
    );
  }
  // The profile defaults to Terminal view, so a Structured view prefill has to survive it.
  await page.route(/\/api\/settings(\?.*)?$/, (r) => r.fulfill({ json: { acp: { structured_view: "terminal" } } }));
  await openStructuredSession(page, mock);
  const banner = page.getByTestId("container-gone-banner");
  await expect(banner.getByRole("heading", { name: "Claude couldn't start in Structured view" })).toBeVisible();
  await expect(banner).toContainText(
    "This Session's sandbox container is gone. Recreate it to use this Session again.",
  );
  return { banner, recreates };
}

const composer = (page: Page) => page.locator("[data-session-composer] textarea");

const noHorizontalScroll = (page: Page) =>
  page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test.describe("at 375px", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("Recreate container confirms, then hands over to the startup line", async ({ page }) => {
    const { banner, recreates } = await openGoneSession(page);
    await expect(banner.getByRole("button")).toHaveText([
      "Recreate container",
      "New Session in story-container-gone",
      "Details",
    ]);
    await expect(page.getByTestId("startup-error-banner")).toHaveCount(0);
    await expect(page.getByText("Claude Code isn't running.")).toBeVisible();
    // A send would start the Session, rebuilding the container without the dialog's warning.
    await expect(composer(page)).toBeDisabled();
    await expect(composer(page)).toHaveAttribute("placeholder", "Recreate the container to send messages.");

    const recreate = banner.getByRole("button", { name: "Recreate container" });
    const bannerBox = (await banner.boundingBox())!;
    const recreateBox = (await recreate.boundingBox())!;
    const newBox = (await banner.getByRole("button", { name: /^New Session in/ }).boundingBox())!;
    expect(recreateBox.width).toBeGreaterThan(bannerBox.width - 40);
    expect(newBox.y).toBeGreaterThan(recreateBox.y);
    expect(await noHorizontalScroll(page)).toBeLessThanOrEqual(0);

    await recreate.click();
    const dialog = page.getByRole("dialog", { name: "Recreate container" });
    await expect(dialog).toContainText(TITLE);
    expect(recreates).toEqual([]);
    await page.getByTestId("recreate-container-confirm").click();
    await expect(dialog).toBeHidden();
    expect(recreates).toEqual(["POST"]);
    await expect(page.getByTestId("startup-wait-line")).toBeVisible();
    await expect(banner).toBeHidden();
    await expect(composer(page)).toBeEnabled();
  });

  test("New Session opens the wizard for this Project, Agent and Group in Structured view", async ({ page }) => {
    const { banner } = await openGoneSession(page);
    await banner.getByRole("button", { name: "New Session in story-container-gone" }).click();
    const w = wizard(page);
    await expect(w.getByTestId("wizard-project-row")).toContainText(`/tmp/${TITLE}`);
    await expect(w.getByTestId("wizard-agent-row")).toContainText("claude");
    await expect(w.getByRole("radio", { name: "Structured view" })).toHaveAttribute("aria-checked", "true");
    await expect(w.getByRole("combobox", { name: "Group" })).toHaveValue("infra");
    await expect(w.getByPlaceholder("Auto-generated if empty")).toHaveValue("");
  });
});

test.describe("at 1280px", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("a read-only dashboard asks the owner and offers no actions", async ({ page }) => {
    const { banner } = await openGoneSession(page, { readOnly: true });
    await expect(banner.getByTestId("container-gone-owner")).toHaveText("Ask the owner to recreate the container.");
    await expect(banner.getByRole("button")).toHaveText(["Details"]);
    await expect(composer(page)).toHaveAttribute("placeholder", "Ask the owner to recreate the container.");
    await banner.getByRole("button", { name: "Details" }).click();
    await expect(banner.locator("pre")).toHaveText(MESSAGE);
  });
});

test("New Session names the Project as its sidebar header does", async ({ page }) => {
  await page.addInitScript((path) => {
    window.localStorage.setItem("aoe-repo-appearance-v1", JSON.stringify({ [path]: { alias: "Infra stack" } }));
  }, `/tmp/${TITLE}`);
  const { banner } = await openGoneSession(page);
  await expect(banner.getByRole("button", { name: "New Session in Infra stack" })).toBeVisible();
});

test("a queued message can't start the Session either", async ({ page }) => {
  await openGoneSession(page, { queued: true });
  const action = page.getByTestId("queued-send-now");
  await expect(action).toHaveAttribute("aria-disabled", "true");
  await expect(action).toHaveAttribute("aria-label", "Recreate the container to send messages.");
});

test("an untyped startup error keeps the composer, as on main", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: TITLE,
    workerState: "absent",
    acpSessionId: "acp-1",
    sessionFields: { is_sandboxed: true, acp_agent: "claude" },
    initialEvents: [{ AgentStartupError: { message: MESSAGE } }],
  });
  await openStructuredSession(page, mock);
  await expect(page.getByTestId("startup-error-banner")).toBeVisible();
  await expect(composer(page)).toBeEnabled();
});
