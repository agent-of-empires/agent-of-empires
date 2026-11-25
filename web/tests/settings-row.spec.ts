// #5302: each schema row carries its section.field key with a copy button. With a mouse the key is invisible until
// the row is hovered or focused, but keeps its line so rows never jump; on a phone it always shows. jsdom can't
// evaluate the hover, focus or breakpoint rules, so this spec checks them in a browser. SettingRow.test.tsx pins
// the rest of the row.

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import { copiedBy } from "./helpers/clipboard";
import type { Locator, Page } from "@playwright/test";

const SHOTS = process.env.SETTINGS_ROW_SHOTS;
const ELEV = { policy: "requires_elevation", reason: "sandbox config affects host isolation" };

// Labels, summaries and flags copied from the real `#[setting]` attributes.
const SCHEMA = [
  {
    section: "sandbox",
    field: "enabled_by_default",
    label: "Run new Sessions in a sandbox",
    widget: { kind: "toggle" },
    web_write: ELEV,
    summary:
      "Each Session's Agent runs inside its own container. It can reach the Project's files and any folders from your computer you add under Extra folders.",
  },
  {
    section: "sandbox",
    field: "default_image",
    label: "Image",
    widget: { kind: "text" },
    web_write: ELEV,
    summary: "The container image Sessions start from.",
  },
  {
    section: "session",
    field: "unread_indicator",
    label: "Unread marker",
    widget: { kind: "toggle" },
    summary: "Mark Sessions with replies you haven't seen.",
  },
  {
    section: "session",
    field: "show_tips",
    label: "Tips",
    widget: { kind: "toggle" },
    summary: "Show an occasional tip in the footer.",
  },
  {
    section: "logging",
    field: "output",
    label: "Write to",
    widget: { kind: "select", options: ["file", "stdout"].map((v) => ({ value: v, label: v })) },
    restart: true,
    summary:
      "The console only works when you run aoe serve in a terminal. The terminal app and the background Daemon always use the file.",
  },
  {
    section: "logging",
    field: "rotation",
    label: "Rotation",
    widget: { kind: "select", options: ["size", "never"].map((v) => ({ value: v, label: v })) },
    restart: true,
  },
].map((d) => ({
  category: d.section,
  description: "",
  web_write: { policy: "allow" },
  profile_overridable: false,
  validation: { rule: "none" },
  advanced: false,
  ...d,
}));

const SETTINGS = {
  sandbox: { enabled_by_default: false, default_image: "ghcr.io/agent-of-empires/sandbox:latest" },
  session: { unread_indicator: true, show_tips: true },
  logging: { output: "file", rotation: "size" },
};

async function open(page: Page, tab: string) {
  await mockSettingsApis(page, { schema: SCHEMA, settings: () => SETTINGS });
  await page.goto(`/settings/${tab}`);
}

const row = (page: Page, key: string) => page.locator(`[data-settings-field="${key}"]`);
const keyText = (page: Page, key: string) => row(page, key).getByText(key, { exact: true });

/** The key line fades in; its box is always laid out. */
async function shown(key: Locator) {
  return (await key.locator("..").evaluate((el) => getComputedStyle(el).opacity)) === "1";
}

const height = async (l: Locator) => (await l.boundingBox())!.height;

async function shot(page: Page, name: string) {
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${name}.png`, fullPage: true });
}

test.describe("with a mouse", () => {
  test.use({ viewport: { width: 1280, height: 800 }, permissions: ["clipboard-read", "clipboard-write"] });

  test("the key shows on hover and on keyboard focus, and copies", async ({ page }) => {
    await open(page, "session");
    const key = keyText(page, "session.show_tips");
    await expect(page.getByRole("switch", { name: "Tips" })).toBeVisible();
    expect(await shown(key)).toBe(false);
    await shot(page, "1280-session");

    // Showing a key moves nothing: the hovered row keeps its height and the row below keeps its place.
    const unread = row(page, "session.unread_indicator");
    const tips = row(page, "session.show_tips");
    const before = [await height(unread), (await tips.boundingBox())!.y];
    await unread.hover();
    await expect.poll(() => shown(keyText(page, "session.unread_indicator"))).toBe(true);
    expect([await height(unread), (await tips.boundingBox())!.y]).toEqual(before);
    expect(await shown(key)).toBe(false);

    await tips.hover();
    await expect.poll(() => shown(key)).toBe(true);
    await shot(page, "1280-session-hover");
    await page.mouse.move(0, 0);
    await expect.poll(() => shown(key)).toBe(false);

    // Tab from the control above reaches this row's copy button even while the key is hidden.
    await page.getByRole("switch", { name: "Unread marker" }).focus();
    const copy = row(page, "session.show_tips").getByRole("button", { name: "Copy config key" });
    for (let i = 0; i < 5 && !(await copy.evaluate((el) => el === document.activeElement)); i++) {
      await page.keyboard.press("Tab");
    }
    await expect(copy).toBeFocused();
    await expect.poll(() => shown(key)).toBe(true);

    expect(await copiedBy(page, () => page.keyboard.press("Enter"))).toBe("session.show_tips");
  });
});

test.describe("on a phone", () => {
  test.use({ viewport: { width: 375, height: 812 }, hasTouch: true, isMobile: true });

  test("the key always shows", async ({ page }) => {
    for (const [tab, key] of [
      ["session", "session.show_tips"],
      ["sandbox", "sandbox.enabled_by_default"],
      ["logging", "logging.rotation"],
    ]) {
      await open(page, tab);
      await expect(keyText(page, key)).toBeVisible();
      expect(await shown(keyText(page, key))).toBe(true);
      await shot(page, `375-${tab}`);
    }
  });
});
