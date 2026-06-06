// Story #3 (#1515): expanding an "Advanced" fold and editing a knob inside it
// persists through the same save-on-change path as any other field.
//
// Drives the real settings UI (not the REST endpoint): land on
// /settings/sandbox, confirm the advanced knobs are folded away by default,
// expand the fold, edit sandbox.cpu_limit, and assert the value reaches the
// profile config and survives a page reload. Sandbox is used (rather than
// Structured view) because `acp` is not an allowed profile-settings section, so
// structured-view knobs do not round-trip through PATCH /api/profiles/{p}/settings.

import { test, expect } from "../helpers/liveTest";

test("sandbox advanced knob edits persist after expanding the fold", async ({
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
  const baseline = (before?.sandbox?.cpu_limit as string | undefined) ?? "";
  const newValue = baseline === "4" ? "2" : "4";

  await page.goto(`${serve.baseUrl}/settings/sandbox`);

  // A high-level control is visible immediately; the advanced knob is folded
  // away by default.
  await expect(page.getByText("Enabled by Default")).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.locator("label", { hasText: /^CPU Limit$/ })).toHaveCount(
    0,
  );

  // Expand the Advanced fold.
  await page
    .getByRole("button", { name: /Advanced/ })
    .first()
    .click();

  const cpuInput = page
    .locator("label", { hasText: /^CPU Limit$/ })
    .locator("..")
    .locator('input[type="text"]');
  await expect(cpuInput).toBeVisible({ timeout: 5_000 });

  // Edit and commit (TextField commits on blur / Enter).
  await cpuInput.fill(newValue);
  await cpuInput.press("Enter");

  // Server-side: PATCH landed against the profile config.
  await expect(async () => {
    const after = await fetch(profileUrl).then((r) => r.json());
    expect(after?.sandbox?.cpu_limit).toBe(newValue);
  }).toPass({ timeout: 5_000 });

  // Frontend-side: after reload the fold is collapsed again (component-local,
  // not persisted), and re-expanding shows the persisted value.
  await page.reload();
  await expect(page.getByText("Enabled by Default")).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.locator("label", { hasText: /^CPU Limit$/ })).toHaveCount(
    0,
  );

  await page
    .getByRole("button", { name: /Advanced/ })
    .first()
    .click();
  await expect(cpuInput).toHaveValue(newValue, { timeout: 5_000 });
});

// The other three folded tabs (Worktree, Structured view, Logging) each render their
// advanced fields only once the fold is expanded. Drive each one in the
// browser so the relocated field markup is exercised end to end (the unit
// suite asserts the same hide/expand behavior; this is the real-DOM pass).
test("worktree, structured-view, and logging advanced folds expand in the browser", async ({
  serve,
  page,
}) => {
  const cases: Array<{ tab: string; anchor: string; field: RegExp }> = [
    // Worktree is schema-driven (#1692): labels + the advanced fold come from
    // the settings schema, so they match the TUI ("Enabled by Default",
    // "Bare Repo Template") rather than the old hand-written web copy.
    {
      tab: "worktree",
      anchor: "Enabled by Default",
      field: /^Bare Repo Template$/,
    },
    {
      tab: "structured-view",
      anchor: "Show tool-call durations",
      field: /^Replay buffer bytes$/,
    },
    {
      tab: "logging",
      anchor: "Default level",
      field: /^Output \(restart req\.\)$/,
    },
  ];

  for (const { tab, anchor, field } of cases) {
    await page.goto(`${serve.baseUrl}/settings/${tab}`);
    await expect(page.getByText(anchor).first()).toBeVisible({
      timeout: 10_000,
    });

    // Folded away by default.
    await expect(page.locator("label", { hasText: field })).toHaveCount(0);

    await page
      .getByRole("button", { name: /Advanced/ })
      .first()
      .click();
    await expect(page.locator("label", { hasText: field })).toBeVisible({
      timeout: 5_000,
    });
  }
});
