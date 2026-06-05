// Profile setting override: changing a setting under a non-default profile
// goes through `PATCH /api/profiles/<name>/settings`, persists under that
// profile, and leaves the global settings under `PATCH /api/settings`
// untouched. Proves per-profile state isolation on the server.
//
// Pairs with profile-lifecycle.spec.ts.

import { test, expect } from "../helpers/liveTest";

test("per-profile setting override leaves global state untouched", async ({
  serve,
  page,
}) => {
  // Seed: create a `work` profile through the public API so the test
  // focuses on the override flow, not creation.
  const createRes = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "work" }),
  });
  expect(createRes.ok).toBeTruthy();

  // Capture baselines for the default-profile read and the work-profile
  // read. The work read should mirror defaults until we patch it.
  const globalBefore: { session?: { default_tool?: string | null } } = await fetch(
    `${serve.baseUrl}/api/settings`,
  ).then((r) => r.json());
  const workBefore: { session?: { default_tool?: string | null } } = await fetch(
    `${serve.baseUrl}/api/profiles/work/settings`,
  ).then((r) => r.json());

  const sentinel = "claude-code-override-test";
  expect(globalBefore?.session?.default_tool).not.toBe(sentinel);
  expect(workBefore?.session?.default_tool).not.toBe(sentinel);

  // Spy on outgoing requests: a successful override must hit
  // PATCH /api/profiles/work/settings exactly once and must not touch the
  // global PATCH /api/settings endpoint.
  const patches: Array<{ url: string; method: string }> = [];
  page.on("request", (req) => {
    const method = req.method();
    const url = req.url();
    if (method !== "PATCH") return;
    if (!url.includes("/api/")) return;
    patches.push({ url, method });
  });

  // Land on the Session settings tab and pick the `work` profile.
  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();

  const profileSelect = page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
  await profileSelect.selectOption("work");
  await expect(profileSelect).toHaveValue("work");

  // Change `session.default_tool` under `work`. The TextField commits on
  // blur via onChange, which fires onSave -> PATCH .../profiles/work/settings.
  const defaultAgentInput = page
    .locator("label", { hasText: /^Default Tool$/ })
    .locator("..")
    .locator("input[type=text]");
  await defaultAgentInput.fill(sentinel);
  await defaultAgentInput.blur();

  // Server-side: work profile picked up the override.
  await expect(async () => {
    const after = await fetch(
      `${serve.baseUrl}/api/profiles/work/settings`,
    ).then((r) => r.json());
    expect(after?.session?.default_tool).toBe(sentinel);
  }).toPass({ timeout: 5_000 });

  // Global profile: unchanged. This is the isolation property the issue
  // asks us to assert. Normalize both sides to `null` because the server
  // omits the field entirely when it has no value.
  const globalAfter: { session?: { default_tool?: string | null } } = await fetch(
    `${serve.baseUrl}/api/settings`,
  ).then((r) => r.json());
  expect(globalAfter?.session?.default_tool ?? null).toBe(
    globalBefore?.session?.default_tool ?? null,
  );
  expect(globalAfter?.session?.default_tool ?? null).not.toBe(sentinel);

  // Request shape: only the per-profile endpoint saw a PATCH.
  expect(
    patches.some((p) => p.url.endsWith("/api/profiles/work/settings")),
  ).toBe(true);
  expect(patches.some((p) => p.url.endsWith("/api/settings"))).toBe(false);
});
