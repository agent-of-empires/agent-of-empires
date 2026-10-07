// Profiles settings tab (/settings/profiles): create, set-default, the retired
// /profiles redirect, and read-only mode, against a stateful in-route store.
// Descriptions, deep links, and the hooks panel are pinned in ProfilesSection.test.tsx.

import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";

interface ProfileState {
  name: string;
  is_default: boolean;
  description: string;
}

interface ProfilesPageMockHandle {
  profiles: ProfileState[];
  posts: Array<{ name?: string }>;
  defaultPatches: Array<{ name?: string }>;
  readOnly: boolean;
}

async function installProfilesPageMocks(
  page: Page,
  opts: { profiles?: string[]; readOnly?: boolean } = {},
): Promise<ProfilesPageMockHandle> {
  const names = opts.profiles ?? ["main"];
  const handle: ProfilesPageMockHandle = {
    profiles: names.map((name, i) => ({ name, is_default: i === 0, description: "" })),
    posts: [],
    defaultPatches: [],
    readOnly: !!opts.readOnly,
  };

  await mockSettingsApis(page, { about: () => ({ read_only: handle.readOnly }) });

  await page.route(
    (url) => url.pathname === "/api/profiles",
    (route) => {
      if (route.request().method() === "POST") {
        const body = route.request().postDataJSON() as { name?: string };
        handle.posts.push(body);
        if (body?.name) handle.profiles.push({ name: body.name, is_default: false, description: "" });
        return route.fulfill({ json: { ok: true } });
      }
      return route.fulfill({
        json: handle.profiles.map(({ name, is_default, description }) => ({ name, is_default, description })),
      });
    },
  );
  await page.route(
    (url) => /^\/api\/profiles\/[^/]+\/settings$/.test(url.pathname),
    (route) => {
      const name = decodeURIComponent(new URL(route.request().url()).pathname.split("/")[3]);
      const profile = handle.profiles.find((p) => p.name === name);
      return route.fulfill({ json: { description: profile?.description ?? "" } });
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

async function openProfiles(page: Page) {
  await page.goto("/settings/profiles");
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();
}

test("+ New profile POSTs and the rail gains the row; Set as default PATCHes and moves the badge", async ({ page }) => {
  const handle = await installProfilesPageMocks(page);
  await openProfiles(page);

  await page.getByRole("button", { name: "+ New profile" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("work");
  await nameInput.press("Enter");

  await expect.poll(() => handle.posts).toEqual([{ name: "work" }]);
  await expect(page.getByRole("button", { name: "work", exact: true })).toBeVisible();
  expect(handle.profiles.map((p) => p.name).sort()).toEqual(["main", "work"]);

  await page.getByRole("button", { name: "work", exact: true }).click();
  await page.getByRole("button", { name: "Set as default" }).click();
  await expect.poll(() => handle.defaultPatches).toEqual([{ name: "work" }]);
  // The page re-fetches the list after the PATCH; the badge follows the flag.
  await expect(page.getByRole("button", { name: "work default" })).toBeVisible();
  expect(handle.profiles.find((p) => p.name === "main")?.is_default).toBe(false);
});

test("the retired /profiles route redirects into the Settings Profiles tab", async ({ page }) => {
  await installProfilesPageMocks(page);
  await page.goto("/profiles");
  await expect(page).toHaveURL(/\/settings\/profiles$/);
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();

  // The redirect preserves any query string (e.g. a ?profile= deep link).
  await page.goto("/profiles?profile=work");
  await expect(page).toHaveURL(/\/settings\/profiles\?profile=work$/);
});

test("read-only mode hides every mutation control", async ({ page }) => {
  await installProfilesPageMocks(page, { profiles: ["main", "work"], readOnly: true });
  await openProfiles(page);
  await page.getByRole("button", { name: "work", exact: true }).click();

  // Scope to the section: the Settings header's ProfileSelector has its own
  // create/rename/delete controls (tracked separately) that we are not asserting here.
  const section = page.getByTestId("profiles-section");
  await expect(section.getByPlaceholder("What this profile is for")).toBeVisible();
  await expect(section.getByRole("button", { name: "+ New profile" })).toHaveCount(0);
  await expect(section.getByRole("button", { name: "Set as default" })).toHaveCount(0);
  await expect(section.getByRole("button", { name: "Rename" })).toHaveCount(0);
  await expect(section.getByRole("button", { name: "Save" })).toHaveCount(0);
});
