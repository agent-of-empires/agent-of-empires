// Dedicated Profiles page (/profiles) against a real `aoe serve`. Covers
// the CRUD + set-default + description round-trips, the deep-link into
// Settings (?profile=), and the read-only hooks panel invariant.
//
// Each test runs against its own fresh serve so profile state is
// deterministic. Pairs with profile-lifecycle.spec.ts, which covers the
// ProfileSelector dropdown inside Settings.

import { test, expect, type ServeHandle } from "../helpers/liveTest";

async function fetchProfiles(
  serve: ServeHandle,
): Promise<Array<{ name: string; is_default: boolean; description?: string }>> {
  const res = await fetch(`${serve.baseUrl}/api/profiles`);
  expect(res.ok).toBeTruthy();
  return res.json();
}

async function seedProfile(serve: ServeHandle, name: string): Promise<void> {
  const res = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  expect(res.ok).toBeTruthy();
}

test("create profile via + New round-trips through POST /api/profiles", async ({
  serve,
  page,
}) => {
  await page.goto(`${serve.baseUrl}/profiles`);
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();

  await page.getByRole("button", { name: "+ New profile" }).click();
  const nameInput = page.getByPlaceholder("Profile name");
  await nameInput.fill("work");
  await nameInput.press("Enter");

  await expect(async () => {
    const profiles = await fetchProfiles(serve);
    expect(profiles.map((p) => p.name).sort()).toEqual(["main", "work"]);
  }).toPass({ timeout: 5_000 });
});

test("set as default round-trips through PATCH /api/default-profile", async ({
  serve,
  page,
}) => {
  await seedProfile(serve, "work");

  await page.goto(`${serve.baseUrl}/profiles`);
  await page.getByRole("button", { name: "work", exact: true }).click();
  await page.getByRole("button", { name: "Set as default" }).click();

  await expect(async () => {
    const profiles = await fetchProfiles(serve);
    expect(profiles.find((p) => p.name === "work")?.is_default).toBe(true);
  }).toPass({ timeout: 5_000 });
});

test("description edit persists across reload", async ({ serve, page }) => {
  await seedProfile(serve, "work");

  await page.goto(`${serve.baseUrl}/profiles`);
  // Selecting a profile fires an async load (getProfileSettings +
  // fetchSettings) whose .then resets the description field to the loaded
  // value. Wait for both responses before typing, otherwise a late resolve
  // clobbers the typed value back to empty and Save PATCHes null.
  await Promise.all([
    page.waitForResponse(
      (r) =>
        r.url().includes("/api/profiles/work/settings") &&
        r.request().method() === "GET",
    ),
    page.waitForResponse((r) => /\/api\/settings(\?|$)/.test(r.url())),
    page.getByRole("button", { name: "work", exact: true }).click(),
  ]);

  const desc = page.getByPlaceholder("What this profile is for");
  await desc.fill("client repos");
  await expect(desc).toHaveValue("client repos");
  await page.getByRole("button", { name: "Save" }).click();

  await expect(async () => {
    const profiles = await fetchProfiles(serve);
    expect(profiles.find((p) => p.name === "work")?.description).toBe(
      "client repos",
    );
  }).toPass({ timeout: 5_000 });

  await page.reload();
  await page.getByRole("button", { name: "work", exact: true }).click();
  await expect(page.getByPlaceholder("What this profile is for")).toHaveValue(
    "client repos",
  );
});

test("Edit configuration deep-links into Settings scoped to the profile", async ({
  serve,
  page,
}) => {
  await seedProfile(serve, "work");

  await page.goto(`${serve.baseUrl}/profiles`);
  await page.getByRole("button", { name: "work", exact: true }).click();
  await page.getByRole("button", { name: /^Session/ }).click();

  await expect(page).toHaveURL(/\/settings\/session\?profile=work/);
  const profileSelect = page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
  await expect(profileSelect).toHaveValue("work");
});

test("opens from the sidebar footer and closes back to the dashboard", async ({
  serve,
  page,
}) => {
  await page.goto(serve.baseUrl);
  await page.getByRole("button", { name: "Profiles", exact: true }).click();
  await expect(page).toHaveURL(/\/profiles$/);
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();

  await page.getByRole("button", { name: "Close" }).click();
  await expect(page).not.toHaveURL(/\/profiles/);
});

test("lifecycle hooks render read-only with the explain-why note", async ({
  serve,
  page,
}) => {
  await page.goto(`${serve.baseUrl}/profiles`);
  await page.getByRole("button", { name: "main" }).click();

  const panel = page.locator("section", { hasText: "Lifecycle hooks" }).first();
  await expect(panel).toBeVisible();
  await expect(panel.getByText(/remote code execution/i)).toBeVisible();
  // Read-only invariant: the hooks panel exposes no form controls.
  await expect(panel.locator("input, textarea, button, select")).toHaveCount(0);
});
