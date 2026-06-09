// Dedicated Profiles page (/profiles): CRUD + set-default + description
// round-trips, the deep-link into Settings (?profile=), the sidebar-footer
// entry point, the read-only mode, and the hooks panel invariant. Ported from
// live to the mocked suite: a stateful in-route profile store stands in for
// the backend, so "persists" assertions check the store and the post-reload
// UI rather than a real config file.
//
// Component-level handler details (the hooks-never-PATCHed invariant across
// every interaction, the in-flight description edit race) are pinned in
// ProfilesPage.test.tsx; this spec covers the app-level wiring: routing,
// the /api/about read_only flag, and the dropdown/rail refresh loops.

import { test, expect } from "./helpers/mockedTest";
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
  /** Recorded PATCH /api/profiles/<name>/settings bodies. */
  settingsPatches: Array<{ profile: string; body: Record<string, unknown> }>;
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
    settingsPatches: [],
    readOnly: !!opts.readOnly,
  };

  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) =>
      r.fulfill({
        json: { read_only: handle.readOnly, auth_mode: "none", behind_tunnel: false, profile: "main" },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) => r.fulfill({ json: [] }),
  );
  // Global settings: ProfilesPage reads `hooks` from here to build the
  // inherited rows of the read-only hooks panel.
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: { hooks: { on_launch: ["echo global-hook"] } } }),
  );

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
      if (route.request().method() === "PATCH") {
        const body = route.request().postDataJSON() as Record<string, unknown>;
        handle.settingsPatches.push({ profile: name, body });
        if (profile && typeof body.description === "string") profile.description = body.description;
        return route.fulfill({ json: { ok: true } });
      }
      return route.fulfill({
        json: {
          description: profile?.description ?? "",
          // The GET deliberately includes hooks (reads are unfiltered) so the
          // panel has something profile-scoped to render.
          hooks: { on_create: ["echo profile-hook"] },
        },
      });
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
  await page.goto("/profiles");
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();
}

test("create profile via + New profile POSTs and the rail gains the row", async ({ page }) => {
  const handle = await installProfilesPageMocks(page);
  await openProfiles(page);

  await page.getByRole("button", { name: "+ New profile" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("work");
  await nameInput.press("Enter");

  await expect.poll(() => handle.posts).toEqual([{ name: "work" }]);
  await expect(page.getByRole("button", { name: "work", exact: true })).toBeVisible();
  expect(handle.profiles.map((p) => p.name).sort()).toEqual(["main", "work"]);
});

test("set as default PATCHes /api/default-profile and moves the badge", async ({ page }) => {
  const handle = await installProfilesPageMocks(page, { profiles: ["main", "work"] });
  await openProfiles(page);

  await page.getByRole("button", { name: "work", exact: true }).click();
  await page.getByRole("button", { name: "Set as default" }).click();

  await expect.poll(() => handle.defaultPatches).toEqual([{ name: "work" }]);
  // The page re-fetches the list after the PATCH; the badge follows the flag.
  await expect(page.getByRole("button", { name: "work default" })).toBeVisible();
  expect(handle.profiles.find((p) => p.name === "main")?.is_default).toBe(false);
});

test("description edit PATCHes only `description` and survives a reload", async ({ page }) => {
  const handle = await installProfilesPageMocks(page, { profiles: ["main", "work"] });
  await openProfiles(page);
  await page.getByRole("button", { name: "work", exact: true }).click();

  const desc = page.getByPlaceholder("What this profile is for");
  await desc.fill("client repos");
  await page.getByRole("button", { name: "Save" }).click();

  await expect.poll(() => handle.settingsPatches).toEqual([{ profile: "work", body: { description: "client repos" } }]);

  await page.reload();
  await page.getByRole("button", { name: "work", exact: true }).click();
  await expect(page.getByPlaceholder("What this profile is for")).toHaveValue("client repos");
});

test("Edit configuration deep-links into Settings scoped to the profile", async ({ page }) => {
  await installProfilesPageMocks(page, { profiles: ["main", "work"] });
  await openProfiles(page);

  await page.getByRole("button", { name: "work", exact: true }).click();
  await page.getByRole("button", { name: /^Session/ }).click();

  await expect(page).toHaveURL(/\/settings\/session\?profile=work/);
  const profileSelect = page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
  await expect(profileSelect).toHaveValue("work");
});

test("opens from the sidebar footer and closes back to the dashboard", async ({ page }) => {
  await installProfilesPageMocks(page);
  await page.goto("/");
  await page.getByRole("button", { name: "Profiles", exact: true }).click();
  await expect(page).toHaveURL(/\/profiles$/);
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();

  await page.getByRole("button", { name: "Close" }).click();
  await expect(page).not.toHaveURL(/\/profiles/);
});

test("read-only mode hides every mutation control", async ({ page }) => {
  await installProfilesPageMocks(page, { profiles: ["main", "work"], readOnly: true });
  await openProfiles(page);
  await page.getByRole("button", { name: "work", exact: true }).click();

  await expect(page.getByPlaceholder("What this profile is for")).toBeVisible();
  await expect(page.getByRole("button", { name: "+ New profile" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Set as default" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Rename" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Save" })).toHaveCount(0);
});

test("lifecycle hooks render read-only with the explain-why note", async ({ page }) => {
  await installProfilesPageMocks(page);
  await openProfiles(page);
  await page.getByRole("button", { name: /^main/ }).click();

  const panel = page.locator("section", { hasText: "Lifecycle hooks" }).first();
  await expect(panel).toBeVisible();
  await expect(panel.getByText(/remote code execution/i)).toBeVisible();
  await expect(panel.getByText("echo profile-hook")).toBeVisible();
  await expect(panel.getByText("echo global-hook")).toBeVisible();
  // Read-only invariant: the hooks panel exposes no form controls.
  await expect(panel.locator("input, textarea, button, select")).toHaveCount(0);
});
