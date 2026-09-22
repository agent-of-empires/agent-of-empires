// Profile create, rename, default, and delete through Settings against a stateful mock. Separate tests because
// SettingsView fetches profiles once and would show stale default options after ProfileSelector edits.

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
  renames: Array<{ from: string; body: { new_name?: string } }>;
  deletes: string[];
  defaultPatches: Array<{ name?: string }>;
}

async function installProfileMocks(page: Page, initial: string[] = ["main"]): Promise<ProfileMockHandle> {
  const handle: ProfileMockHandle = {
    profiles: initial.map((name, i) => ({ name, is_default: i === 0 })),
    posts: [],
    renames: [],
    deletes: [],
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
    (url) => /^\/api\/profiles\/[^/]+\/rename$/.test(url.pathname),
    (route) => {
      const from = decodeURIComponent(new URL(route.request().url()).pathname.split("/")[3]);
      const body = route.request().postDataJSON() as { new_name?: string };
      handle.renames.push({ from, body });
      const p = handle.profiles.find((x) => x.name === from);
      if (p && body?.new_name) p.name = body.new_name;
      return route.fulfill({ json: { ok: true } });
    },
  );
  await page.route(
    (url) => /^\/api\/profiles\/[^/]+$/.test(url.pathname),
    (route) => {
      if (route.request().method() !== "DELETE") return route.fulfill({ status: 405 });
      const name = decodeURIComponent(new URL(route.request().url()).pathname.split("/")[3]);
      handle.deletes.push(name);
      handle.profiles = handle.profiles.filter((p) => p.name !== name);
      return route.fulfill({ json: { ok: true } });
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

test("rename profile via Rename PATCHes .../rename and the selection follows", async ({ page }) => {
  const handle = await installProfileMocks(page, ["main", "work"]);
  await openSessionSettings(page);

  await profileSelect(page).selectOption("work");
  await expect(profileSelect(page)).toHaveValue("work");

  await page.getByRole("button", { name: "Rename" }).click();
  const renameInput = page.getByPlaceholder("New name");
  await renameInput.fill("clients");
  await renameInput.press("Enter");

  await expect.poll(() => handle.renames).toEqual([{ from: "work", body: { new_name: "clients" } }]);
  await expect(profileSelect(page)).toHaveValue("clients");
  expect(handle.profiles.map((p) => p.name).sort()).toEqual(["clients", "main"]);
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

test("delete profile via Delete issues DELETE /api/profiles/<name>", async ({ page }) => {
  const handle = await installProfileMocks(page, ["main", "scratch"]);
  page.on("dialog", (d) => d.accept());
  await openSessionSettings(page);

  // Delete is hidden for the default profile.
  await profileSelect(page).selectOption("scratch");
  await expect(profileSelect(page)).toHaveValue("scratch");

  await page.getByRole("button", { name: "Delete" }).click();

  await expect.poll(() => handle.deletes).toEqual(["scratch"]);
  expect(handle.profiles.map((p) => p.name)).toEqual(["main"]);
  await expect(profileSelect(page)).toHaveValue("main");
  await expect(profileSelect(page).locator("option")).toHaveCount(1);
});

test("invalid profile name: client validation blocks POST /api/profiles", async ({ page }) => {
  const handle = await installProfileMocks(page);
  await openSessionSettings(page);

  await page.getByRole("button", { name: "+ New" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("bad name");
  await nameInput.press("Enter");

  await expect(page.getByText("Only letters, digits, hyphens, and underscores")).toBeVisible();
  expect(handle.posts).toEqual([]);
  expect(handle.profiles.map((p) => p.name)).toEqual(["main"]);
});
