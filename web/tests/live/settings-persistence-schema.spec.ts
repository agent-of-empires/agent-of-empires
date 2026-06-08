// Schema-driven settings round-trip through the rendered UI (#1792 story 1).
//
// The other settings-persistence specs PATCH the REST endpoint directly; this
// one drives the generic SchemaSection control in the browser and asserts the
// edit reaches the profile config and survives a reload. Tmux is used because
// it is a fully non-elevation, profile-overridable section (every field is a
// plain `allow` select), so it is the cleanest end-to-end round-trip for the
// schema renderer (Worktree, the first migrated section, is elevation-gated,
// so its live coverage stays render-only).

import { test, expect } from "../helpers/liveTest";

test("a schema-driven select persists through the UI and a reload", async ({
  serve,
  page,
}) => {
  const profiles: Array<{ name: string; is_default?: boolean }> = await fetch(
    `${serve.baseUrl}/api/profiles`,
  ).then((r) => r.json());
  const defaultProfile =
    profiles.find((p) => p.is_default)?.name ?? profiles[0]?.name ?? "main";
  const profileUrl = `${serve.baseUrl}/api/profiles/${encodeURIComponent(defaultProfile)}/settings`;

  const before = await fetch(profileUrl).then((r) => r.json());
  const baseline = (before?.tmux?.status_bar as string | undefined) ?? "auto";
  const next = baseline === "enabled" ? "disabled" : "enabled";

  await page.goto(`${serve.baseUrl}/settings/tmux`);

  const statusBar = page
    .locator("label", { hasText: /^Status Bar$/ })
    .locator("..")
    .locator("select");
  await expect(statusBar).toBeVisible({ timeout: 10_000 });

  // Schema-driven SelectField saves on change.
  await statusBar.selectOption(next);

  // Server-side: the edit reached the profile config.
  await expect(async () => {
    const after = await fetch(profileUrl).then((r) => r.json());
    expect(after?.tmux?.status_bar).toBe(next);
  }).toPass({ timeout: 5_000 });

  // Frontend-side: the persisted value is read back after a reload.
  await page.reload();
  const statusBarAfter = page
    .locator("label", { hasText: /^Status Bar$/ })
    .locator("..")
    .locator("select");
  await expect(statusBarAfter).toHaveValue(next, { timeout: 10_000 });
});
