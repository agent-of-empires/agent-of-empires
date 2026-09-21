// Single-screen new-session wizard DSL (#2210), shared by mocked and live specs. Queries scope to the
// modal because the app shell behind it owns colliding labels ("More options", "New session").

import { expect, type Locator, type Page, type Route } from "@playwright/test";
import { sessionResponse } from "./sessions";

export function wizard(page: Page): Locator {
  return page.getByTestId("session-wizard");
}

export async function openWizard(page: Page) {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByTestId("session-wizard")).toBeVisible();
}

/** Pick a recent or saved project by a substring of its path or name. */
export async function selectProject(page: Page, pathText: string) {
  const recent = wizard(page).getByRole("button").filter({ hasText: pathText }).first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
}

export async function selectAgent(page: Page, name: string | RegExp) {
  await wizard(page).getByRole("button", { name }).click();
}

/** Expand the "More options" fold if it is not already open. */
export async function expandMoreOptions(page: Page) {
  const button = wizard(page).getByRole("button", { name: "More options" });
  await button.waitFor({ state: "visible" });
  if ((await button.getAttribute("aria-expanded")) !== "true") await button.click();
}

export async function setTitle(page: Page, title: string) {
  await wizard(page).getByPlaceholder("Auto-generated if empty").fill(title);
}

export async function launch(page: Page) {
  await wizard(page)
    .getByRole("button", { name: /Launch session/ })
    .click();
}

export const CLAUDE_AGENT = { name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" };

export function sessionStub(overrides: Record<string, unknown> = {}) {
  return sessionResponse({
    id: "seed-session",
    title: "seed",
    project_path: "/tmp/example",
    group_path: "/tmp",
    ...overrides,
  });
}

export interface WizardMockOptions {
  agents?: unknown[];
  profiles?: unknown[];
  /** Default `/api/settings` body; `worktree.enabled` drives the "Create a worktree" default (#2423). */
  settings?: Record<string, unknown>;
  /** Per-profile settings keyed by the `?profile=` query param. */
  profileSettings?: Record<string, Record<string, unknown>>;
  docker?: boolean;
  projects?: unknown[];
  sessions?: unknown[];
  /** Handle the create POST; return undefined for the default success response. */
  onCreate?: (body: Record<string, unknown>, route: Route) => Promise<unknown> | unknown;
}

/** Mock every API the dashboard and wizard read. Returns the create-session POST bodies in order. */
export async function mockWizardApis(page: Page, opts: WizardMockOptions = {}) {
  const created: Record<string, unknown>[] = [];
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["themes", "groups", "devices"])
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: [] }));
  for (const path of ["about", "system/update-status"])
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: {} }));
  await page.route("**/api/settings**", (r) => {
    const profile = new URL(r.request().url()).searchParams.get("profile");
    return r.fulfill({ json: (profile && opts.profileSettings?.[profile]) || opts.settings || {} });
  });
  await page.route("**/api/profiles", (r) => r.fulfill({ json: opts.profiles ?? [] }));
  await page.route("**/api/recent-projects", (r) => r.fulfill({ json: { projects: [] } }));
  await page.route("**/api/projects**", (r) => r.fulfill({ json: opts.projects ?? [] }));
  await page.route("**/api/docker/status", (r) =>
    r.fulfill({ json: { available: !!opts.docker, runtime: opts.docker ? "docker" : null } }),
  );
  await page.route("**/api/agents", (r) => r.fulfill({ json: opts.agents ?? [CLAUDE_AGENT] }));
  await page.route("**/api/sessions", async (r) => {
    if (r.request().method() !== "POST") {
      return r.fulfill({ json: { sessions: opts.sessions ?? [sessionStub()], workspace_ordering: [] } });
    }
    const body = JSON.parse(r.request().postData() || "{}");
    created.push(body);
    if ((await opts.onCreate?.(body, r)) === undefined) return r.fulfill({ json: { session: { id: "new-session" } } });
  });
  return created;
}

/** Mock the APIs, open the dashboard at a desktop size, and optionally open the wizard on the seeded project. */
export async function startWizard(page: Page, opts: WizardMockOptions & { project?: boolean } = {}) {
  const created = await mockWizardApis(page, opts);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/");
  if (opts.project !== false) {
    await openWizard(page);
    await selectProject(page, "/tmp/example");
  }
  return created;
}
