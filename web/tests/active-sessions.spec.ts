import { test, expect } from "./helpers/mockedTest";

for (const width of [390, 1280]) {
  test(`only active sessions are shown without clearing work at ${width}px`, async ({ page }) => {
    const writes: string[] = [];
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const sessions = [
      ["working", "Running"],
      ["waiting", "Waiting"],
      ["failed", "Error"],
      ["finished", "Idle"],
      ["stopped", "Stopped"],
      ["favorite-pa", "Idle"],
    ].map(([id, status]) => ({
      id,
      title: id,
      status,
      project_path: "/tmp/activity-proof",
      group_path: "",
      tool: "claude",
      yolo_mode: false,
      created_at: "2026-01-01T00:00:00Z",
      last_accessed_at: null,
      idle_entered_at: null,
      last_error: null,
      branch: null,
      main_repo_path: null,
      is_sandboxed: false,
      favorited: id === "favorite-pa",
      has_terminal: true,
      profile: "default",
      workspace_repos: [],
      archived_at: null,
      snoozed_until: null,
      trashed_at: null,
    }));
    await page.route("**/api/**", (route) => {
      const req = route.request();
      if (req.method() !== "GET") {
        writes.push(req.url());
        return route.fulfill({ status: 409 });
      }
      const path = new URL(req.url()).pathname;
      if (path === "/api/login/status") return route.fulfill({ json: { required: false, authenticated: true } });
      if (path === "/api/sessions") return route.fulfill({ json: { sessions, workspace_ordering: [] } });
      if (path === "/api/plugins/ui-state") return route.fulfill({ json: { entries: [], notifications: [] } });
      if (path === "/api/settings") return route.fulfill({ json: { app_state: { has_seen_web_tour: true } } });
      if (path === "/api/about")
        return route.fulfill({ json: { read_only: false, auth_mode: "none", profile: "default" } });
      if (path === "/api/docker/status") return route.fulfill({ json: {} });
      if (
        ["/api/themes", "/api/agents", "/api/profiles", "/api/groups", "/api/devices", "/api/projects"].includes(path)
      )
        return route.fulfill({ json: [] });
      return route.fulfill({ status: 404, json: { error: "not_found" } });
    });
    await page.setViewportSize({ width, height: 844 });
    await page.goto("/");
    if (width < 768) await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await page.getByRole("button", { name: "Active sessions", exact: true }).click();
    const rows = page.getByTestId("sidebar-session-row");
    await expect(rows).toHaveCount(4);
    await expect(rows.filter({ hasText: "finished" })).toHaveCount(0);
    await expect(rows.filter({ hasText: "favorite-pa" })).toHaveCount(1);
    for (let axis = 0; axis < 4; axis++) {
      await page.getByTestId("sidebar-axis-toggle").click();
      await expect(rows).toHaveCount(4);
    }
    await page.getByRole("button", { name: "All sessions", exact: true }).click();
    await expect(rows).toHaveCount(6);
    await page.reload();
    if (width < 768) await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(rows).toHaveCount(6);
    await page.getByRole("button", { name: "Active sessions", exact: true }).click();
    await expect(rows).toHaveCount(4);
    await expect(page.getByRole("button", { name: "Active sessions", exact: true })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(page.getByRole("button", { name: "All sessions", exact: true })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
    expect(writes.filter((url) => !["/api/presence", "/api/telemetry/seen"].includes(new URL(url).pathname))).toEqual(
      [],
    );
    expect(errors).toEqual([]);
    await page.screenshot({ path: `/tmp/aoe-active-view-${width}.png`, animations: "disabled" });
  });
}
