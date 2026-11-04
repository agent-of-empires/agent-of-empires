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

// A dashboard whose /api/about never answered can't tell a read-only server from a writable one, so it hides
// write actions behind a Retry notice; the wizard's own served-profile retry is covered in SessionWizard.test.tsx.
test("a failed about hides write actions behind a Retry notice, then the wizard uses the served profile", async ({
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
  const notice = page.getByRole("alert").filter({ hasText: "Couldn't load this server's settings" });
  await expect(notice).toBeVisible();
  await expect(page.getByRole("button", { name: /New session Pick a project/i })).toHaveCount(0);
  expect(reads).toEqual([]);
  available = true;
  await notice.getByRole("button", { name: "Retry" }).click();
  await expect(notice).toHaveCount(0);
  await openWizard(page);
  const w = wizard(page);
  await expect(w.getByRole("button", { name: "Retry server profile" })).toHaveCount(0);
  // With no project chosen yet, the wizard opens on its project picker.
  await w.getByRole("button", { name: "Recent", exact: true }).click();
  await selectProject(page, "/tmp/example");
  await openPanel(page, "Extra repos");
  await expect(
    w.getByTestId("extra-repos-picker").getByRole("button").filter({ hasText: "alpha-extra" }),
  ).toBeVisible();
  await w.getByRole("button", { name: "Done" }).click();
  await setTitle(page, "after-retry");
  await w.getByRole("button", { name: /Launch session/ }).click();
  await expect.poll(() => created.length).toBe(1);
  expect(created[0]).toMatchObject({ title: "after-retry", path: "/tmp/example" });
  expect(created[0]!.profile).toBeUndefined();
  expect(reads).toContain("Alpha");
  expect(reads).not.toContain("Main");
});
