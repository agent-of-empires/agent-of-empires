// Profile lifecycle: create, select, rename, set default, delete, driven
// through the dashboard UI (ProfileSelector inside the Session tab of
// SettingsView plus the "Default profile" SelectField). Ported from live to
// the mocked suite: a stateful in-route profile store stands in for the
// backend, so each assertion checks both the request the UI emitted and that
// the dropdowns reflect the mutated list after the component's re-fetch.
//
// Split into independent tests because SettingsView's `profiles` state is
// fetched once on mount and only refreshes when its own `handleSetDefault`
// fires, so a UI chain that mixes ProfileSelector edits with the Default
// profile dropdown picks up stale options without a page reload.
//
// The backend's own persistence of these mutations is a server contract; the
// component-level validation branches live in ProfileSelector.test.tsx.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";

interface ProfileState {
  name: string;
  is_default: boolean;
}

interface ProfileMockHandle {
  profiles: ProfileState[];
  /** Recorded POST /api/profiles bodies. */
  posts: Array<{ name?: string }>;
  /** Recorded renames as { from, body }. */
  renames: Array<{ from: string; body: { new_name?: string } }>;
  /** Recorded DELETE /api/profiles/<name> targets. */
  deletes: string[];
  /** Recorded PATCH /api/default-profile bodies. */
  defaultPatches: Array<{ name?: string }>;
}

/** Stateful stubs for everything the Settings session tab touches. Mutations
 *  update `handle.profiles`, so the GET the component re-issues after each
 *  action returns the post-mutation list and the dropdowns must follow. */
async function installProfileMocks(page: Page, initial: string[] = ["main"]): Promise<ProfileMockHandle> {
  const handle: ProfileMockHandle = {
    profiles: initial.map((name, i) => ({ name, is_default: i === 0 })),
    posts: [],
    renames: [],
    deletes: [],
    defaultPatches: [],
  };

  // Keep the sessions poll green so SettingsView's offline guard does not
  // disable the content fieldset (the Default profile select lives inside it).
  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) =>
      r.fulfill({
        json: { read_only: false, auth_mode: "none", behind_tunnel: false, profile: "main" },
      }),
  );
  // Schema may be empty: the session tab's Default profile selector is the
  // one non-schema row and renders regardless.
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) => r.fulfill({ json: [] }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: { session: {} } }),
  );

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
  // ProfileSelector reloads its list after a successful create.
  await expect(profileSelect(page).locator('option[value="work"]')).toHaveCount(1);
  expect(handle.profiles.find((p) => p.name === "main")?.is_default).toBe(true);
  expect(handle.profiles.find((p) => p.name === "work")?.is_default).toBe(false);
});

test("rename profile via Rename PATCHes .../rename and the selection follows", async ({ page }) => {
  const handle = await installProfileMocks(page, ["main", "work"]);
  await openSessionSettings(page);

  // Select `work` so Rename targets it (rename acts on the selectedProfile).
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
  // handleSetDefault re-fetches; the dropdown now reflects the new default.
  await expect(defaultSelect).toHaveValue("work");
});

test("delete profile via Delete issues DELETE /api/profiles/<name>", async ({ page }) => {
  const handle = await installProfileMocks(page, ["main", "scratch"]);
  // Auto-accept the native confirm() dialog the component pops.
  page.on("dialog", (d) => d.accept());
  await openSessionSettings(page);

  // Select the non-default profile so Delete is visible (the component hides
  // it for the default row, and the server rejects deleting the active one).
  await profileSelect(page).selectOption("scratch");
  await expect(profileSelect(page)).toHaveValue("scratch");

  await page.getByRole("button", { name: "Delete" }).click();

  await expect.poll(() => handle.deletes).toEqual(["scratch"]);
  expect(handle.profiles.map((p) => p.name)).toEqual(["main"]);
  // The selection falls back to the default profile after the reload.
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
