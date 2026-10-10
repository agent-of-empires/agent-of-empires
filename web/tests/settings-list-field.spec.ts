// A list setting (e.g. a plugin `string_list`) must let touch and keyboard
// users edit and remove entries; the remove control used to appear on hover
// only and there was no way to edit an entry. A stateful store stands in for
// the backend so saved edits round-trip through the PATCH.

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import type { Locator, Page } from "@playwright/test";

const SCHEMA = [
  {
    section: "sandbox",
    field: "environment",
    label: "Environment",
    widget: { kind: "list" },
    category: "sandbox",
    description: "",
    advanced: false,
    profile_overridable: true,
    validation: { rule: "none" },
    web_write: { policy: "allow" },
  },
];

async function installMocks(page: Page, initial: string[]) {
  const store: { sandbox: { environment: string[] }; patches: unknown[] } = {
    sandbox: { environment: initial },
    patches: [],
  };
  await mockSettingsApis(page, { schema: SCHEMA, settings: () => ({ sandbox: store.sandbox }) });
  await page.route(
    (url) => url.pathname === "/api/settings",
    (route) => {
      if (route.request().method() !== "PATCH") return route.fallback();
      const body = route.request().postDataJSON() as { sandbox: { environment: string[] } };
      store.patches.push(body);
      store.sandbox = { ...store.sandbox, ...body.sandbox };
      return route.fulfill({ json: { ok: true } });
    },
  );
  return store;
}

const entry = (page: Page, text: string) => page.locator("span.font-mono", { hasText: new RegExp(`^${text}$`) });

for (const [name, use, touch] of [
  ["desktop", {}, false],
  ["touch", { viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true }, true],
] as const) {
  test.describe(name, () => {
    test.use(use);
    const press = (l: Locator) => (touch ? l.tap() : l.click());

    test("edit, save and cancel list entries with visible controls", async ({ page }) => {
      const store = await installMocks(page, ["A=1", "B=2", "C=3"]);
      await page.goto("/settings/sandbox");

      // Controls are visible without hover.
      await expect(page.getByTitle("Edit B=2")).toBeVisible();
      await expect(page.getByTitle(/^Remove /).first()).toBeVisible();

      await press(page.getByTitle("Edit B=2"));
      const input = page.getByRole("textbox");
      await expect(input).toHaveValue("B=2");
      await input.fill("B=20");
      await input.press("Enter");
      await expect.poll(() => store.sandbox.environment).toEqual(["A=1", "B=20", "C=3"]);
      await expect(entry(page, "B=20")).toBeVisible();

      // Cancel discards the draft without saving.
      await press(page.getByTitle("Edit C=3"));
      await page.getByRole("textbox").fill("C=30");
      await press(page.getByRole("button", { name: "Cancel" }));
      await expect(entry(page, "C=3")).toBeVisible();
      expect(store.patches).toHaveLength(1);

      // Removing an earlier entry mid-edit keeps editing the same entry.
      await press(page.getByTitle("Edit C=3"));
      await page.getByRole("textbox").fill("C=31");
      await press(page.getByTitle(/^Remove /).first());
      await expect.poll(() => store.sandbox.environment).toEqual(["B=20", "C=3"]);
      await expect(page.getByRole("textbox")).toHaveValue("C=31");
      await press(page.getByRole("button", { name: "Save" }));
      await expect.poll(() => store.sandbox.environment).toEqual(["B=20", "C=31"]);
    });
  });
}
