import type { Page } from "@playwright/test";
import { expect, test } from "./helpers/mockedTest";

interface MockSession {
  id: string;
  title: string;
  context_resume?:
    | { state: "available" }
    | { state: "indeterminate"; reason: "runtime_check_required" }
    | { state: "unavailable"; reason: "no_target" };
}

async function mockApis(page: Page, sessions: MockSession[]) {
  await page.route("**/api/login/status", (route) => route.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (route) => {
    if (route.request().method() !== "GET") return route.fulfill({ status: 400 });
    return route.fulfill({
      json: {
        sessions: sessions.map((session) => ({
          ...session,
          project_path: `/tmp/${session.id}`,
          group_path: "",
          tool: "claude",
          status: "Idle",
          yolo_mode: false,
          created_at: "2026-09-04T00:00:00Z",
          last_accessed_at: null,
          idle_entered_at: null,
          last_error: null,
          branch: null,
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

test("surfaces context resume risks without cluttering healthy rows", async ({ page }) => {
  await mockApis(page, [
    { id: "missing", title: "Missing target", context_resume: { state: "unavailable", reason: "no_target" } },
    {
      id: "runtime",
      title: "Runtime check",
      context_resume: { state: "indeterminate", reason: "runtime_check_required" },
    },
    { id: "available", title: "Available context", context_resume: { state: "available" } },
    { id: "old-daemon", title: "Unreported context" },
  ]);
  await page.goto("/");

  await expect(page.getByLabel("Context resume unavailable: no resume target has been captured")).toHaveText("ctx:no");
  await expect(page.getByLabel("Context resume not yet confirmed: a runtime check is required at launch")).toHaveText(
    "ctx:check",
  );
  await expect(page.getByRole("link", { name: /Available context/ })).not.toContainText("ctx:");
  await expect(page.getByRole("link", { name: /Unreported context/ })).not.toContainText("ctx:");
});
