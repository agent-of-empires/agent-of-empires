import type { Page, Route } from "@playwright/test";
import { test, expect, publishedRequests } from "./helpers/mockedTest";
import { mockWizardApis, openWizard, openPanel, selectProject, setTitle, wizard } from "./helpers/wizard";

async function bodyCount(page: Page, path: string) {
  return page.evaluate(
    (path) => (window as typeof window & { __mockedBodies: string[] }).__mockedBodies.filter((p) => p === path).length,
    path,
  );
}
async function consume(page: Page, route: Route, json: unknown) {
  const path = new URL(route.request().url()).pathname;
  const before = await bodyCount(page, path);
  await route.fulfill({ json });
  await expect.poll(() => bodyCount(page, path)).toBeGreaterThan(before);
}
async function failRead(page: Page, route: Route) {
  const path = new URL(route.request().url()).pathname;
  const failures = () =>
    page.evaluate(
      (path) =>
        (window as typeof window & { __mockedFailures: string[] }).__mockedFailures.filter((p) => p === path).length,
      path,
    );
  const before = await failures();
  await route.fulfill({ status: 503 });
  await expect.poll(failures).toBeGreaterThan(before);
}
async function focus(page: Page) {
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
}
const profiles = [
  { name: "Main", is_default: true },
  { name: "Alpha", is_default: false },
  { name: "Beta", is_default: false },
];
const settings = {
  Alpha: { session: { default_tool: "claude", yolo_mode_default: false }, worktree: { enabled: false } },
  Beta: { session: { default_tool: "claude", yolo_mode_default: true }, worktree: { enabled: true } },
};

test("focus refresh retains the acknowledged registry on failure and closes writes on served-profile change", async ({
  page,
}) => {
  await mockWizardApis(page, { servedProfile: "Alpha", profiles, sessions: [] });
  let served = "Alpha";
  let hold = false;
  const reads: Route[] = [];
  const alpha = { name: "alpha-only", path: "/tmp/alpha-only", scope: "profile", pinned: true };
  const beta = { name: "beta-only", path: "/tmp/beta-only", scope: "profile", pinned: true };
  await page.route("**/api/about", (r) => r.fulfill({ json: { profile: served } }));
  await page.route("**/api/projects**", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    if (hold) {
      reads.push(r);
      return;
    }
    return r.fulfill({ json: [alpha] });
  });
  await page.goto("/");
  const oldHeader = page.getByTestId("sidebar-group-header").filter({ hasText: "alpha-only" });
  await expect(oldHeader).toBeVisible();
  hold = true;
  await focus(page);
  await expect.poll(() => reads.length).toBe(1);
  expect(new URL(reads[0]!.request().url()).searchParams.get("profile")).toBe("Alpha");
  await oldHeader.click({ button: "right" });
  await expect(page.getByTestId("sidebar-group-context-menu-unpin")).toBeVisible();
  await page.keyboard.press("Escape");
  await failRead(page, reads.shift()!);
  await expect(oldHeader).toBeVisible();
  await oldHeader.click({ button: "right" });
  await expect(page.getByTestId("sidebar-group-context-menu-unpin")).toBeVisible();
  await page.keyboard.press("Escape");
  await focus(page);
  await expect.poll(() => reads.length).toBe(1);
  await expect(oldHeader).toBeVisible();
  await oldHeader.click({ button: "right" });
  await expect(page.getByTestId("sidebar-group-context-menu-unpin")).toBeVisible();
  await page.keyboard.press("Escape");
  await consume(page, reads.shift()!, [alpha]);
  served = "Beta";
  await focus(page);
  await expect.poll(() => reads.length).toBe(1);
  expect(new URL(reads[0]!.request().url()).searchParams.get("profile")).toBe("Beta");
  await expect(oldHeader).toHaveCount(0);
  await expect(page.getByTestId("sidebar-group-context-menu-unpin")).toHaveCount(0);
  await failRead(page, reads.shift()!);
  await focus(page);
  await expect.poll(() => reads.length).toBe(1);
  await expect(oldHeader).toHaveCount(0);
  expect((await publishedRequests(page, "/api/projects", "POST")).length).toBe(0);
  await consume(page, reads.shift()!, [beta]);
  const newHeader = page.getByTestId("sidebar-group-header").filter({ hasText: "beta-only" });
  await expect(newHeader).toBeVisible();
  await newHeader.click({ button: "right" });
  await expect(page.getByTestId("sidebar-group-context-menu-unpin")).toBeVisible();
});

test("wizard retries failed about without guessing the machine default or overwriting dirty fields", async ({
  page,
}) => {
  const created = await mockWizardApis(page, {
    profiles,
    sessions: [],
    profileSettings: settings,
    projects: [{ name: "example", path: "/tmp/example", scope: "profile" }],
  });
  await page.route("**/api/projects**", (r) => {
    if (new URL(r.request().url()).pathname !== "/api/projects" || r.request().method() !== "GET") return r.fallback();
    const profile = new URL(r.request().url()).searchParams.get("profile");
    return r.fulfill({
      json:
        profile === "Alpha"
          ? [
              { name: "example", path: "/tmp/example", scope: "profile" },
              { name: "alpha-extra", path: "/tmp/alpha-extra", scope: "profile" },
            ]
          : profile === "Beta"
            ? [
                { name: "beta-only", path: "/tmp/beta-only", scope: "profile" },
                { name: "beta-extra", path: "/tmp/beta-extra", scope: "profile" },
              ]
            : [],
    });
  });
  let available = false;
  const reads: string[] = [];
  await page.route("**/api/about", (r) =>
    available ? r.fulfill({ json: { profile: "Alpha" } }) : r.fulfill({ status: 503 }),
  );
  await page.route("**/api/settings**", (r) => {
    const profile = new URL(r.request().url()).searchParams.get("profile");
    if (profile) reads.push(profile);
    return r.fulfill({ json: profile === "Beta" ? settings.Beta : settings.Alpha });
  });
  await page.goto("/");
  await openWizard(page);
  const w = wizard(page);
  const retry = w.getByRole("button", { name: "Retry server profile" });
  await expect(retry).toBeEnabled();
  // Open the form without requiring a project registry: scratch is scope-free.
  await w.getByRole("button", { name: "Scratch", exact: true }).click();
  await w.getByRole("button", { name: "Use a scratch folder" }).click();
  await setTitle(page, "dirty-title");
  await w.getByRole("switch", { name: "Auto-approve actions" }).click();
  await expect(w.getByRole("button", { name: /Launch session/ })).toBeDisabled();
  expect(reads).toEqual([]);
  expect((await publishedRequests(page, "/api/projects", "GET")).length).toBe(0);
  available = true;
  await retry.click();
  await expect(w.getByRole("button", { name: /Launch session/ })).toBeEnabled();
  await expect(w.getByPlaceholder("Auto-generated if empty")).toHaveValue("dirty-title");
  await expect(w.getByRole("switch", { name: "Auto-approve actions" })).toHaveAttribute("aria-checked", "true");
  expect(reads).toContain("Alpha");
  expect(reads).not.toContain("Main");
  await openPanel(page, "Project");
  await w.getByRole("button", { name: "Recent", exact: true }).click();
  await selectProject(page, "/tmp/example");
  await openPanel(page, "Extra repos");
  await expect(
    w.getByTestId("extra-repos-picker").getByRole("button").filter({ hasText: "alpha-extra" }),
  ).toBeVisible();
  await expect(w.getByTestId("extra-repos-picker").getByText("beta-extra")).toHaveCount(0);
  await w.getByRole("button", { name: "Done" }).click();
  await w.getByRole("button", { name: /Launch session/ }).click();
  await expect.poll(() => created.length).toBe(1);
  expect(created[0]).toMatchObject({ title: "dirty-title", path: "/tmp/example", yolo_mode: true });
  expect(created[0]!.profile).toBeUndefined();
});

test("wizard switching explicit Beta back to unresolved Server default resets Beta defaults after about retry", async ({
  page,
}) => {
  const created = await mockWizardApis(page, {
    profiles,
    sessions: [],
    profileSettings: settings,
    projects: [{ name: "example", path: "/tmp/example", scope: "profile" }],
  });
  await page.route("**/api/projects**", (r) => {
    if (new URL(r.request().url()).pathname !== "/api/projects" || r.request().method() !== "GET") return r.fallback();
    const profile = new URL(r.request().url()).searchParams.get("profile");
    return r.fulfill({
      json:
        profile === "Alpha"
          ? [
              { name: "example", path: "/tmp/example", scope: "profile" },
              { name: "alpha-extra", path: "/tmp/alpha-extra", scope: "profile" },
            ]
          : profile === "Beta"
            ? [
                { name: "beta-only", path: "/tmp/beta-only", scope: "profile" },
                { name: "beta-extra", path: "/tmp/beta-extra", scope: "profile" },
              ]
            : [],
    });
  });
  let available = false;
  let holdNextAbout = true;
  let heldAbout: Route | undefined;
  await page.route("**/api/about", (r) => {
    if (!available) return r.fulfill({ status: 503 });
    if (holdNextAbout) {
      holdNextAbout = false;
      heldAbout = r;
      return;
    }
    return r.fulfill({ json: { profile: "Alpha" } });
  });
  page.on("dialog", (dialog) => dialog.accept());
  await page.goto("/");
  await openWizard(page);
  const w = wizard(page);
  await expect(w.getByRole("button", { name: "Retry server profile" })).toBeEnabled();
  await w.getByRole("button", { name: "Done" }).click();
  await openPanel(page, "Profile");
  await w.getByRole("radio", { name: /^Beta/ }).click();
  await expect(w.getByRole("switch", { name: "Auto-approve actions" })).toHaveAttribute("aria-checked", "true");
  await expect(w.getByRole("switch", { name: "Create a worktree" })).toHaveAttribute("aria-checked", "true");
  await openPanel(page, "Project");
  await selectProject(page, "/tmp/beta-only");
  await openPanel(page, "Extra repos");
  await expect(w.getByTestId("extra-repos-picker").getByRole("button").filter({ hasText: "beta-extra" })).toBeVisible();
  await expect(w.getByTestId("extra-repos-picker").getByText("alpha-extra")).toHaveCount(0);
  await w.getByRole("button", { name: "Done" }).click();
  await setTitle(page, "kept-across-profile-switch");
  available = true;
  await openPanel(page, "Profile");
  await w.getByRole("radio", { name: /Server default/ }).click();
  await expect.poll(() => Boolean(heldAbout)).toBe(true);
  await expect(w.getByRole("button", { name: /Launch session/ })).toBeDisabled();
  await consume(page, heldAbout!, { profile: "Alpha" });
  await expect(w.getByRole("button", { name: /Launch session/ })).toBeEnabled();
  await expect(w.getByRole("switch", { name: "Auto-approve actions" })).toHaveAttribute("aria-checked", "false");
  await expect(w.getByRole("switch", { name: "Create a worktree" })).toHaveAttribute("aria-checked", "false");
  await expect(w.getByPlaceholder("Auto-generated if empty")).toHaveValue("kept-across-profile-switch");
  await openPanel(page, "Project");
  await expect(w.getByRole("button").filter({ hasText: "/tmp/beta-only" })).toHaveCount(0);
  await selectProject(page, "/tmp/example");
  await openPanel(page, "Extra repos");
  await expect(
    w.getByTestId("extra-repos-picker").getByRole("button").filter({ hasText: "alpha-extra" }),
  ).toBeVisible();
  await expect(w.getByTestId("extra-repos-picker").getByText("beta-extra")).toHaveCount(0);
  await w.getByRole("button", { name: "Done" }).click();
  await w.getByRole("button", { name: /Launch session/ }).click();
  await expect.poll(() => created.length).toBe(1);
  expect(created[0]).toMatchObject({ path: "/tmp/example", yolo_mode: false, worktree_enabled: false });
  expect(created[0]!.profile).toBeUndefined();
});
