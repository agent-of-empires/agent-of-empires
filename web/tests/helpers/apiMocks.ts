// Boot endpoints every mocked spec has to stub before the dashboard renders.

import type { Page } from "@playwright/test";

const STATIC_PATHS = ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"];

/** `login/status` plus the config GETs the shell fetches on mount. Later
 *  `page.route` registrations win, so a spec can override any of these after. */
export async function mockStaticApis(page: Page, overrides: Record<string, unknown> = {}) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of STATIC_PATHS) {
    const json = path in overrides ? overrides[path] : path === "docker/status" ? {} : [];
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json }));
  }
}

/** The per-session no-ops: terminal attach, an empty diff, and silent sockets. */
export async function mockSessionShellApis(page: Page) {
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  await page.route("**/api/sessions/*/terminal", (r) => r.fulfill({ status: 200, body: "" }));
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: { files: [], per_repo_bases: [], warning: null } }),
  );
  await page.routeWebSocket(/\/sessions\/.*\/(ws|acp-ws|container-ws)$/, () => {});
}

/** The reads SettingsView and the profiles page make before they render. A
 *  failing sessions poll would disable the settings fieldset. */
export async function mockSettingsApis(
  page: Page,
  opts: {
    about?: () => Record<string, unknown>;
    profiles?: () => unknown;
    schema?: unknown;
    settings?: () => unknown;
  } = {},
) {
  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) =>
      r.fulfill({
        json: { read_only: false, auth_mode: "none", behind_tunnel: false, profile: "main", ...opts.about?.() },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/profiles",
    (r) => r.fulfill({ json: opts.profiles?.() ?? [{ name: "main", is_default: true }] }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) => r.fulfill({ json: opts.schema ?? [] }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: opts.settings?.() ?? {} }),
  );
}
