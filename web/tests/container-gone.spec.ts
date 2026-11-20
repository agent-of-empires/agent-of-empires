// The startup error banner for a sandboxed Session whose container is gone (#5652).

import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";
import { wizard } from "./helpers/wizard";

const TITLE = "story-container-gone";
const MESSAGE = "This Session's sandbox container is gone. Recreate it or start a new Session and container.";
// A Session that ran before, past the compaction reminder's threshold.
const REMINDER_ABOUT = { acp_compaction_reminder: true, acp_compaction_reminder_percent: 75 };
const WITH_COMPACT = { available_commands: [{ name: "compact", description: "compact", accepts_input: false }] };
const NEAR_FULL = [{ UsageUpdated: { usage: { used: 180_000, size: 200_000, cost: null } } }];

const RECREATE_LINE = "This Session's sandbox container is gone. Recreate it to use this Session again.";

async function openGoneSession(
  page: Page,
  {
    readOnly = false,
    queued = false,
    nearFull = false,
    fields = {} as Record<string, unknown>,
    line = RECREATE_LINE,
  } = {},
) {
  const mock = await mockAcpSession(page, {
    title: TITLE,
    about: { ...(readOnly ? { read_only: true } : {}), ...(nearFull ? REMINDER_ABOUT : {}) },
    reducedState: nearFull ? WITH_COMPACT : undefined,
    workerState: "absent",
    acpSessionId: "acp-1",
    sessionFields: { is_sandboxed: true, group_path: "infra", acp_agent: "claude", ...fields },
    initialEvents: [
      ...(nearFull ? NEAR_FULL : []),
      { AgentStartupError: { message: MESSAGE, cause: { kind: "container_missing" } } },
    ],
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
  await expect(banner.getByTestId("container-gone-line")).toHaveText(line);
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
    const { banner } = await openGoneSession(page, {
      readOnly: true,
      line: "This Session's sandbox container is gone. Ask the owner to recreate the container.",
    });
    await expect(banner.getByRole("button")).toHaveText(["Details"]);
    await expect(composer(page)).toHaveAttribute("placeholder", "Ask the owner to recreate the container.");
    await banner.getByRole("button", { name: "Details" }).click();
    await expect(banner.locator("pre")).toHaveText(MESSAGE);
  });
});

const AT = "2026-11-01T00:00:00Z";
const NEW = `New Session in ${TITLE}`;
// Recreate isn't offered for these, so the screen names what is: Unarchive, Restore, or waiting.
const UNOFFERED = [
  {
    state: "archived",
    fields: { archived_at: AT },
    line: "Unarchive this Session from the sidebar, then recreate its container.",
    buttons: [NEW, "Details"],
    composer: null,
  },
  {
    state: "trashed",
    fields: { trashed_at: AT },
    line: "Restore this Session from Trash, then recreate its container.",
    buttons: ["Restore", NEW, "Details"],
    composer: null,
  },
  {
    state: "being created",
    fields: { status: "Creating" },
    line: "Wait for this Session to finish being created.",
    buttons: [NEW, "Details"],
    composer: "Wait for this Session to finish being created.",
  },
  {
    state: "being deleted",
    fields: { status: "Deleting" },
    line: "Wait for this Session to finish being deleted.",
    buttons: [NEW, "Details"],
    composer: "Wait for this Session to finish being deleted.",
  },
];

test.describe("at 1280px, where Recreate isn't offered", () => {
  test.use({ viewport: { width: 1280, height: 800 } });
  for (const c of UNOFFERED) {
    test(`a Session ${c.state} names the control that exists`, async ({ page }) => {
      const { banner } = await openGoneSession(page, {
        fields: c.fields,
        line: `This Session's sandbox container is gone. ${c.line}`,
      });
      await expect(banner.getByRole("button")).toHaveText(c.buttons);
      if (c.composer === null) await expect(composer(page)).toHaveCount(0);
      else await expect(composer(page)).toHaveAttribute("placeholder", c.composer);
    });
  }

  test("Restore restores the trashed Session", async ({ page }) => {
    const restores: string[] = [];
    await page.route("**/api/sessions/*/restore", (r) => {
      restores.push(r.request().method());
      return r.fulfill({ status: 500, json: {} });
    });
    const { banner } = await openGoneSession(page, {
      fields: { trashed_at: AT },
      line: "This Session's sandbox container is gone. Restore this Session from Trash, then recreate its container.",
    });
    await banner.getByRole("button", { name: "Restore" }).click();
    await expect.poll(() => restores).toEqual(["POST"]);
    // A failed restore brings the button back.
    await expect(banner.getByRole("button", { name: "Restore" })).toBeEnabled();
  });
});

test.describe("at 375px, where Recreate isn't offered", () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test("the mobile pane names the control that exists too", async ({ page }) => {
    const { banner } = await openGoneSession(page, {
      fields: { status: "Deleting" },
      line: "This Session's sandbox container is gone. Wait for this Session to finish being deleted.",
    });
    await expect(banner.getByRole("button")).toHaveText([NEW, "Details"]);
    await expect(composer(page)).toHaveAttribute("placeholder", "Wait for this Session to finish being deleted.");
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

test("the compaction reminder's Compact now can't start the Session", async ({ page }) => {
  await openGoneSession(page, { nearFull: true });
  // The untyped case below shows the reminder from the same events, so its absence here is the block.
  await expect(page.getByTestId("compaction-reminder")).toHaveCount(0);
});

test("an untyped startup error keeps the composer and the compaction reminder, as on main", async ({ page }) => {
  const mock = await mockAcpSession(page, {
    title: TITLE,
    about: REMINDER_ABOUT,
    reducedState: WITH_COMPACT,
    workerState: "absent",
    acpSessionId: "acp-1",
    sessionFields: { is_sandboxed: true, acp_agent: "claude" },
    initialEvents: [...NEAR_FULL, { AgentStartupError: { message: MESSAGE } }],
  });
  await openStructuredSession(page, mock);
  await expect(page.getByTestId("startup-error-banner")).toBeVisible();
  await expect(composer(page)).toBeEnabled();
  await expect(page.getByTestId("compaction-reminder").getByRole("button", { name: "Compact now" })).toBeVisible();
});
