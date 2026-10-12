import { test, expect, authHeaders } from "../helpers/liveTest";

test("/api/about reports read_only=true", async ({ serveReadOnly }) => {
  const about = await fetch(`${serveReadOnly.baseUrl}/api/about`).then((r) => r.json());
  expect(about?.read_only).toBe(true);
});

test("read-only guards take precedence over malformed mutation bodies", async ({ serveReadOnly }) => {
  const endpoints = [
    ["POST", "/api/sessions"],
    ["POST", "/api/skills"],
    ["POST", "/api/plugins/commands/absent/invoke"],
    ["POST", "/api/plugins/absent/update/apply"],
    ["POST", "/api/plugins/absent/update/dismiss"],
    ["POST", "/api/plugins/absent/enabled"],
    ["POST", "/api/plugins/install/preview"],
    ["POST", "/api/plugins/install"],
    ["POST", "/api/sessions/0000000000000000/acp/switch-agent"],
    ["DELETE", "/api/sessions/0000000000000000"],
    ["DELETE", "/api/workspaces"],
    ["POST", "/api/sessions/0000000000000000/trash"],
    ["POST", "/api/plugins/absent/action"],
    ["POST", "/api/plugins/absent/worker/restart"],
    ["POST", "/api/plugins/absent/uninstall"],
    ["POST", "/api/skills/sync"],
    ["POST", "/api/skills/local/absent/adopt"],
    ["PUT", "/api/skills/absent"],
    ["DELETE", "/api/skills/absent"],
    ["POST", "/api/mcp/servers/absent/resolve"],
    ["POST", "/api/mcp/servers/absent/keep"],
    ["POST", "/api/mcp/servers/absent/drop"],
    ["POST", "/api/git/clone"],
    ["POST", "/api/profiles"],
    ["DELETE", "/api/profiles/absent"],
    ["PATCH", "/api/profiles/absent/rename"],
    ["PATCH", "/api/profiles/absent/settings"],
    ["PATCH", "/api/default-profile"],
    ["POST", "/api/projects"],
    ["PATCH", "/api/projects/absent"],
    ["DELETE", "/api/projects/absent"],
    ["PATCH", "/api/settings"],
    ["PATCH", "/api/theme"],
    ["POST", "/api/tips/show"],
    ["POST", "/api/app-state/tip-seen"],
    ["POST", "/api/app-state/web-tour-seen"],
    ["PATCH", "/api/app-state/web-ui-state"],
    ["POST", "/api/app-state/dismiss-update"],
    ["POST", "/api/app-state/volume-ignores-globs-acknowledged"],
    ["PUT", "/api/workspace-ordering"],
    ["POST", "/api/telemetry/consent"],
    ["POST", "/api/telemetry/seen"],
    ["POST", "/api/telemetry/structured-interaction"],
    ["PATCH", "/api/sessions/0000000000000000"],
    ["PATCH", "/api/sessions/0000000000000000/group"],
    ["PATCH", "/api/sessions/0000000000000000/notifications"],
    ["PATCH", "/api/sessions/0000000000000000/diff-base"],
    ["PATCH", "/api/sessions/0000000000000000/worktree-name"],
    ["POST", "/api/sessions/0000000000000000/projects"],
    ["PATCH", "/api/sessions/0000000000000000/pin"],
    ["PATCH", "/api/sessions/0000000000000000/color"],
    ["PATCH", "/api/sessions/0000000000000000/archive"],
    ["PATCH", "/api/sessions/0000000000000000/snooze"],
    ["POST", "/api/sessions/0000000000000000/restore"],
    ["PATCH", "/api/sessions/0000000000000000/unread"],
    ["POST", "/api/sessions/0000000000000000/stop"],
    ["POST", "/api/sessions/0000000000000000/smart-rename"],
    ["POST", "/api/sessions/0000000000000000/summarize"],
    ["POST", "/api/sessions/0000000000000000/start"],
    ["POST", "/api/sessions/0000000000000000/terminal"],
    ["DELETE", "/api/sessions/0000000000000000/terminal"],
    ["POST", "/api/sessions/0000000000000000/container-terminal"],
    ["POST", "/api/sessions/0000000000000000/send"],
    ["POST", "/api/sessions/0000000000000000/paste-image"],
    ["POST", "/api/sessions/0000000000000000/acp/spawn"],
    ["DELETE", "/api/sessions/0000000000000000/acp"],
    ["POST", "/api/sessions/0000000000000000/acp/enable"],
    ["POST", "/api/sessions/0000000000000000/acp/disable"],
    ["POST", "/api/sessions/0000000000000000/acp/install-agent"],
    ["POST", "/api/sessions/0000000000000000/acp/switch-provider"],
    ["POST", "/api/sessions/0000000000000000/acp/prompt"],
    ["POST", "/api/sessions/0000000000000000/acp/prompt/diff-comments"],
    ["POST", "/api/sessions/0000000000000000/acp/cancel"],
    ["POST", "/api/sessions/0000000000000000/acp/force_end_turn"],
    ["POST", "/api/sessions/0000000000000000/acp/mode"],
    ["POST", "/api/sessions/0000000000000000/acp/config-option"],
    ["POST", "/api/sessions/0000000000000000/acp/approvals/absent"],
    ["POST", "/api/sessions/0000000000000000/acp/elicitations/absent"],
    ["POST", "/api/sessions/0000000000000000/queue"],
    ["DELETE", "/api/sessions/0000000000000000/queue"],
    ["PATCH", "/api/sessions/0000000000000000/queue/absent"],
    ["DELETE", "/api/sessions/0000000000000000/queue/absent"],
  ];
  const before = await fetch(`${serveReadOnly.baseUrl}/api/sessions`).then((res) => res.json());
  for (const [method, path] of endpoints) {
    for (const body of ["{}", '{"junk":true}', "not even json", undefined]) {
      const response = await fetch(`${serveReadOnly.baseUrl}${path}`, {
        method,
        headers: { "Content-Type": "application/json" },
        body,
      });
      expect(response.status, `${method} ${path} body=${String(body)}`).toBe(403);
      expect(await response.json()).toMatchObject({ error: "read_only" });
    }
  }
  const after = await fetch(`${serveReadOnly.baseUrl}/api/sessions`).then((res) => res.json());
  expect(after).toEqual(before);
});

test("ordinary plugin actions do not require remote-session elevation", async ({ servePreauthed }) => {
  const headers = {
    ...authHeaders(servePreauthed),
    "Content-Type": "application/json",
    "X-Forwarded-For": "203.0.113.10",
  };
  const privileged = await fetch(`${servePreauthed.baseUrl}/api/plugins/absent/enabled`, {
    method: "POST",
    headers,
    body: JSON.stringify({ enabled: true }),
  });
  expect(privileged.status).toBe(403);
  expect(await privileged.json()).toMatchObject({ error: "elevation_required" });
  const ordinary = await fetch(`${servePreauthed.baseUrl}/api/plugins/absent/action`, {
    method: "POST",
    headers,
    body: JSON.stringify({ method: "github.refresh", params: {} }),
  });
  expect(ordinary.status).toBe(404);
  expect(await ordinary.json()).toMatchObject({ error: "no_worker" });
});

test("dashboard suppresses mutation UI in read-only", async ({ serveReadOnly, page }) => {
  // The `n` shortcut's guard reads /api/about, so wait for it before pressing.
  const aboutPromise = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, {
    timeout: 10_000,
  });
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  await expect(page.getByText("This dashboard is in read-only mode.")).toBeVisible();

  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeHidden({ timeout: 2_000 });
});
