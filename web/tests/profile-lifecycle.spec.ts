// Profile create and default through Settings against a stateful mock. Separate tests because SettingsView
// fetches profiles once and would show stale default options after ProfileSelector edits. Rename, delete and
// name validation are pinned in ProfileSelector.test.tsx.

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";

interface ProfileState {
  name: string;
  is_default: boolean;
}

interface ProfileMockHandle {
  profiles: ProfileState[];
  posts: Array<{ name?: string }>;
  defaultPatches: Array<{ name?: string }>;
}

async function installProfileMocks(page: Page, initial: string[] = ["main"]): Promise<ProfileMockHandle> {
  const handle: ProfileMockHandle = {
    profiles: initial.map((name, i) => ({ name, is_default: i === 0 })),
    posts: [],
    defaultPatches: [],
  };

  await mockSettingsApis(page, { settings: () => ({ session: {} }) });

  await page.route(
    (url) => url.pathname === "/api/profiles",
    (route) => {
      if (route.request().method() === "POST") {
        const body = route.request().postDataJSON() as { name?: string };
        handle.posts.push(body);
        if (body?.name) handle.profiles.push({ name: body.name, is_default: false });
        return route.fulfill({ json: { ok: true } });
      }
      return route.fulfill({ json: handle.profiles });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/default-profile",
    (route) => {
      const body = route.request().postDataJSON() as { name?: string };
      handle.defaultPatches.push(body);
      for (const p of handle.profiles) p.is_default = p.name === body?.name;
      return route.fulfill({ json: { ok: true } });
    },
  );

  return handle;
}

function profileSelect(page: Page) {
  return page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
}

async function openSessionSettings(page: Page) {
  await page.goto("/settings/session");
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();
}

test("create profile via + New POSTs /api/profiles and the dropdown gains it", async ({ page }) => {
  const handle = await installProfileMocks(page);
  await openSessionSettings(page);

  await page.getByRole("button", { name: "+ New" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("work");
  await nameInput.press("Enter");

  await expect.poll(() => handle.posts).toEqual([{ name: "work" }]);
  await expect(profileSelect(page).locator('option[value="work"]')).toHaveCount(1);
  expect(handle.profiles.find((p) => p.name === "main")?.is_default).toBe(true);
  expect(handle.profiles.find((p) => p.name === "work")?.is_default).toBe(false);
});

test("set default profile via Default profile dropdown PATCHes /api/default-profile", async ({ page }) => {
  const handle = await installProfileMocks(page, ["main", "work"]);
  await openSessionSettings(page);

  const defaultSelect = page
    .locator("label", { hasText: /^Default profile$/ })
    .locator("..")
    .locator("select");
  await expect(defaultSelect).toHaveValue("main");
  await defaultSelect.selectOption("work");

  await expect.poll(() => handle.defaultPatches).toEqual([{ name: "work" }]);
  await expect(defaultSelect).toHaveValue("work");
});
