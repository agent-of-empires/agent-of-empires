import type { Page } from "@playwright/test";
import type { ContextResumeAvailability, SessionStatus } from "../src/lib/types";
import { expect, test } from "./helpers/mockedTest";

interface MockSession {
  id: string;
  title: string;
  context_resume?: ContextResumeAvailability | { state: string; reason?: string };
  project_path?: string;
  group_path?: string;
  branch?: string | null;
  status?: SessionStatus;
}

async function mockApis(page: Page, sessions: MockSession[]) {
  await page.route("**/api/login/status", (route) => route.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (route) => {
    if (route.request().method() !== "GET") return route.fulfill({ status: 400 });
    return route.fulfill({
      json: {
        sessions: sessions.map((session) => ({
          ...session,
          project_path: session.project_path ?? `/tmp/${session.id}`,
          group_path: session.group_path ?? "",
          tool: "claude",
          status: session.status ?? "Idle",
          yolo_mode: false,
          created_at: "2026-09-04T00:00:00Z",
          last_accessed_at: null,
          idle_entered_at: null,
          last_error: null,
          branch: session.branch ?? null,
          main_repo_path: null,
          is_sandboxed: false,
          has_terminal: true,
          profile: "default",
          workspace_repos: [],
        })),
        workspace_ordering: [],
      },
    });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (route) => route.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

test("surfaces only unavailable context resume states", async ({ page }) => {
  await mockApis(page, [
    { id: "missing", title: "Missing target", context_resume: { state: "unavailable", reason: "no_target" } },
    {
      id: "runtime",
      title: "Runtime check",
      context_resume: { state: "indeterminate", reason: "runtime_check_required" },
    },
    { id: "available", title: "Available context", context_resume: { state: "available" } },
    { id: "old-daemon", title: "Unreported context" },
    {
      id: "future-reason",
      title: "Future reason",
      context_resume: { state: "unavailable", reason: "future_reason" },
    },
    { id: "future-state", title: "Future state", context_resume: { state: "future_state" } },
  ]);
  await page.goto("/");

  await expect(page.getByTitle("Context resume unavailable: no resume target has been captured")).toHaveText("ctx:no");
  await expect(page.getByTitle("Context resume unavailable", { exact: true })).toHaveText("ctx:no");
  await expect(page.getByRole("link", { name: /Missing target/ })).toHaveAccessibleName(/Missing target ctx:no$/);
  for (const title of ["Runtime check", "Available context", "Unreported context", "Future state"]) {
    await expect(page.getByRole("link", { name: new RegExp(title) })).not.toContainText("ctx:");
  }
});

test("uses the active session for a multi-session workspace badge and navigation", async ({ page }) => {
  await mockApis(page, [
    {
      id: "idle",
      title: "Idle session",
      project_path: "/tmp/shared",
      branch: "feature/shared",
      status: "Idle",
      context_resume: { state: "unavailable", reason: "no_target" },
    },
    {
      id: "running",
      title: "Running session",
      project_path: "/tmp/shared",
      branch: "feature/shared",
      status: "Running",
      context_resume: { state: "unavailable", reason: "forced_fresh" },
    },
  ]);
  await page.goto("/");

  await expect(page.getByTitle("Context resume unavailable: the next launch was explicitly reset")).toHaveText(
    "ctx:no",
  );
  await expect(page.getByTitle("Context resume unavailable: no resume target has been captured")).toHaveCount(0);
  const workspaceLink = page.locator('a[href="/session/running"]');
  await expect(workspaceLink).toHaveCount(1);
  await workspaceLink.click();
  await expect(page).toHaveURL(new RegExp("/session/running$"));
});

for (const axis of ["group", "repo+group"]) {
  test(`keeps the badge and activation on the same ${axis} slice`, async ({ page }) => {
    await page.addInitScript((axis) => localStorage.setItem("aoe-sidebar-axis", axis), axis);
    await page.route("**/api/app-state/web-ui-state", (route) => route.fulfill({ json: { "aoe-sidebar-axis": axis } }));
    await mockApis(page, [
      {
        id: "idle",
        title: "Idle session",
        project_path: "/tmp/shared",
        branch: "shared",
        group_path: "A",
        status: "Stopped",
        context_resume: { state: "unavailable", reason: "no_target" },
      },
      {
        id: "running",
        title: "Running session",
        project_path: "/tmp/shared",
        branch: "shared",
        group_path: "B",
        status: "Running",
        context_resume: { state: "available" },
      },
    ]);
    await page.goto("/");
    const row = page.locator('a[href="/session/idle"]');
    await expect(row).toContainText("ctx:no");
    await row.click({ modifiers: ["Control"] });
    await expect(page).toHaveURL(new RegExp("/$"));
    await expect(row).toHaveAttribute("data-selected", "true");
    await row.click();
    await expect(page).toHaveURL(new RegExp("/session/idle$"));
    await page.goto("/");
    await row.focus();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(new RegExp("/session/idle$"));
  });
}
