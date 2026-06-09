// Profile lifecycle: create, select, rename, set default, delete, against
// a real `aoe serve`. Drives the dashboard UI (ProfileSelector inside the
// Session tab of SettingsView plus the "Default profile" SelectField) and
// asserts `GET /api/profiles` reflects each step.
//
// Split into independent tests because SettingsView's `profiles` state is
// fetched once on mount and only refreshes when its own `handleSetDefault`
// fires, so a UI chain that mixes ProfileSelector edits with the Default
// profile dropdown picks up stale options without a page reload. Keeping
// each lifecycle step in its own fresh serve makes each assertion
// deterministic and the surface coverage explicit.
//
// Pairs with profile-override.spec.ts.

import { test, expect, type ServeHandle } from "../helpers/liveTest";

async function fetchProfiles(serve: ServeHandle): Promise<Array<{ name: string; is_default: boolean }>> {
  const res = await fetch(`${serve.baseUrl}/api/profiles`);
  expect(res.ok).toBeTruthy();
  return res.json();
}

test("create profile via + New round-trips through POST /api/profiles", async ({ serve, page }) => {
  const baseline = await fetchProfiles(serve);
  expect(baseline.map((p) => p.name)).toEqual(["main"]);

  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "+ New" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("work");
  await nameInput.press("Enter");

  await expect(async () => {
    const profiles = await fetchProfiles(serve);
    expect(profiles.map((p) => p.name).sort()).toEqual(["main", "work"]);
    expect(profiles.find((p) => p.name === "main")?.is_default).toBe(true);
    expect(profiles.find((p) => p.name === "work")?.is_default).toBe(false);
  }).toPass({ timeout: 5_000 });
});

test("rename profile via Rename button round-trips through PATCH .../rename", async ({ serve, page }) => {
  // Seed: pre-create `work` so the test focuses on the rename flow.
  const seed = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "work" }),
  });
  expect(seed.ok).toBeTruthy();

  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();

  // Select `work` so Rename targets it (rename acts on the selectedProfile).
  const profileSelect = page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
  await profileSelect.selectOption("work");
  await expect(profileSelect).toHaveValue("work");

  await page.getByRole("button", { name: "Rename" }).click();
  const renameInput = page.getByPlaceholder("New name");
  await renameInput.fill("clients");
  await renameInput.press("Enter");

  await expect(async () => {
    const profiles = await fetchProfiles(serve);
    expect(profiles.map((p) => p.name).sort()).toEqual(["clients", "main"]);
  }).toPass({ timeout: 5_000 });
  await expect(profileSelect).toHaveValue("clients");
});

test("set default profile via Default profile dropdown round-trips through PATCH /api/default-profile", async ({
  serve,
  page,
}) => {
  // Seed `work` via the API so the Default profile dropdown options
  // include it on first mount. SettingsView's `profiles` state is fetched
  // once on mount and stays stale through ProfileSelector edits, so
  // seeding upstream of the page load is the deterministic path.
  const seed = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "work" }),
  });
  expect(seed.ok).toBeTruthy();

  // PATCH /api/default-profile writes the disk-stored default that takes
  // effect on the next `aoe serve` startup (see
  // `session::set_default_profile` in src/session/mod.rs). The running
  // server's AppState.profile is fixed at startup, so a PATCH does not
  // re-flag `is_default` on the in-memory GET /api/profiles response.
  // Instead of probing for a state change that the live server doesn't
  // expose, spy on the request itself: the UI must hit the endpoint with
  // the expected body and receive 200.
  const patchPromise = page.waitForResponse(
    (res) => res.url().endsWith("/api/default-profile") && res.request().method() === "PATCH",
  );

  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByText("Default profile", { exact: true })).toBeVisible();

  const defaultSelect = page
    .locator("label", { hasText: /^Default profile$/ })
    .locator("..")
    .locator("select");
  await expect(defaultSelect).toHaveValue("main");
  await defaultSelect.selectOption("work");

  const patchRes = await patchPromise;
  expect(patchRes.ok()).toBe(true);
  expect(patchRes.status()).toBe(200);
  expect(patchRes.request().postDataJSON()).toEqual({ name: "work" });
});

test("delete profile via Delete button round-trips through DELETE /api/profiles/<name>", async ({ serve, page }) => {
  // Auto-accept the native confirm() dialog the component pops.
  page.on("dialog", (d) => d.accept());

  const seed = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "scratch" }),
  });
  expect(seed.ok).toBeTruthy();

  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();

  // Select the non-default profile so Delete is visible (component hides
  // it for the active row, and the server rejects deletion of the active
  // profile in src/server/api/system.rs).
  const profileSelect = page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
  await profileSelect.selectOption("scratch");
  await expect(profileSelect).toHaveValue("scratch");

  const deletePromise = page.waitForResponse(
    (res) => res.url().endsWith("/api/profiles/scratch") && res.request().method() === "DELETE",
    { timeout: 30_000 },
  );

  await page.getByRole("button", { name: "Delete" }).click();

  const deleteRes = await deletePromise;
  expect(deleteRes.ok()).toBe(true);

  const profiles = await fetchProfiles(serve);
  expect(profiles.map((p) => p.name)).toEqual(["main"]);
});

test("invalid profile name: client validation blocks POST /api/profiles", async ({ serve, page }) => {
  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();

  // Intercept POST /api/profiles to detect any leak through validation.
  let posted = false;
  await page.route(`${serve.baseUrl}/api/profiles`, (route) => {
    if (route.request().method() === "POST") {
      posted = true;
    }
    return route.continue();
  });

  await page.getByRole("button", { name: "+ New" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("bad name");
  await nameInput.press("Enter");

  await expect(page.getByText("Only letters, digits, hyphens, and underscores")).toBeVisible();
  expect(posted).toBe(false);

  const profiles = await fetchProfiles(serve);
  expect(profiles.map((p) => p.name)).toEqual(["main"]);
});
