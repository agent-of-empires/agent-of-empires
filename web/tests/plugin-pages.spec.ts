// #4460: plugin pages open full width from the TopBar or palette, framed with
// `sandbox="allow-scripts"` so plugin script never runs on the dashboard origin.

import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis, mockSessionShellApis } from "./helpers/apiMocks";
import { sessionResponse } from "./helpers/sessions";
import type { Page } from "@playwright/test";

const PAGE = {
  plugin_id: "acme.kit",
  id: "board",
  title: "Board",
  icon: "kanban",
  url: "/api/plugins/acme.kit/pages/board",
  capabilities: ["session.read"],
};

// The daemon's page policy (src/server/api/plugins.rs `PLUGIN_PAGE_CSP`).
const PAGE_CSP =
  "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; " +
  "img-src data: blob:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

// Reports what the page could reach back to the dashboard.
const FIXTURE = `<!doctype html><p>board fixture</p><script>
const probe = (f) => { try { return String(f()); } catch (e) { return "blocked:" + e.name; } };
const report = { storage: probe(() => localStorage.getItem("aoe_auth_token")), cookie: probe(() => document.cookie), href: location.href };
fetch("/api/sessions").then(() => "reached", (e) => "blocked:" + e.name).then((api) => parent.postMessage({ ...report, api }, "*"));
</script>`;

// A page driving the bridge the way the docs/plugin-api.md client snippet does.
const BRIDGE_FIXTURE = `<!doctype html><p>bridge fixture</p><script>
let next = 0;
const pending = new Map();
const pushes = [];
const { port1, port2 } = new MessageChannel();
port1.onmessage = (e) => {
  const m = e.data;
  if (m.event) {
    pushes.push(m);
    if (m.event === "theme") parent.postMessage({ kind: "theme", data: m.data }, "*");
    return;
  }
  pending.get(m.id)?.(m);
};
const send = (msg, transfer) => new Promise((resolve) => {
  pending.set(msg.id, resolve);
  if (transfer) parent.postMessage(msg, "*", transfer);
  else port1.postMessage(msg);
});
window.call = (method, params) => send({ aoeBridge: 1, id: ++next, method, params });
(async () => {
  const ready = await send({ aoeBridge: 1, id: ++next, method: "ready", params: { nonce: location.hash.slice(1) } }, [port2]);
  const sessions = await call("sessions.list");
  const uiState = await call("plugins.uiState");
  const action = await call("action", { method: "move", params: { to: "done" }, sessionId: "sess-a" });
  const unknown = await call("sessions.archive");
  parent.postMessage({ kind: "results", ready, sessions, uiState, action, unknown, pushes: pushes.slice(0, 2) }, "*");
})();
</script>`;

// Navigates away before its own load, so the next document's load reads as the frame's first.
const REDIRECT_FIXTURE = `<!doctype html><script>location.replace("/api/plugins/acme.other/pages/board");</script>`;

// Another plugin's page trying to take the redirecting page's bridge without its nonce.
const OTHER_FIXTURE = `<!doctype html><p>other fixture</p><script>
const { port1, port2 } = new MessageChannel();
port1.onmessage = (e) => parent.postMessage({ kind: "other-reply", data: e.data }, "*");
parent.postMessage({ aoeBridge: 1, id: 1, method: "ready", params: { nonce: location.hash.slice(1) } }, "*", [port2]);
parent.postMessage({ kind: "other-sent" }, "*");
</script>`;

const theme = (name: string, appearance: "dark" | "light", bg: string) => ({
  name,
  source: "builtin",
  appearance,
  web: { cssVars: { "--color-surface-900": bg } },
  terminal: { cssVars: {} },
  syntax: { shikiTheme: "github-dark" },
});

async function mockApis(page: Page, pages: unknown[]) {
  await mockStaticApis(page);
  await mockSessionShellApis(page);
  await page.route("**/api/sessions", (r) =>
    r.fulfill({
      json: {
        sessions: [sessionResponse({ id: "sess-a", title: "Ethiopians", project_path: "/tmp/agent-of-empires" })],
        workspace_ordering: [],
      },
    }),
  );
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: [], notifications: [], pages } }));
  await page.route(`**${PAGE.url}/ticket`, (r) => r.fulfill({ status: 204 }));
  await page.route(`**${PAGE.url}`, (r) =>
    r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: FIXTURE }),
  );
}

/** Opens the palette once the first ui-state poll has landed, so a missing entry is not just an early read.
 *  Starts on a session, where no first-run overlay covers the palette. */
async function searchPalette(page: Page, query: string) {
  const polled = page.waitForResponse("**/api/plugins/ui-state");
  await page.goto("/session/sess-a");
  await polled;
  await page.locator("body").click();
  await page.keyboard.press("ControlOrMeta+k");
  const search = page.getByPlaceholder("Search actions, sessions, settings…");
  await expect(search).toBeVisible();
  await search.fill(query);
}

test.describe("Plugin pages (#4460)", () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.addInitScript(() => {
      localStorage.setItem("aoe_auth_token", "secret-token");
      const w = window as typeof window & { __frameReports: unknown[] };
      w.__frameReports = [];
      window.addEventListener("message", (e) => w.__frameReports.push(e.data));
    });
  });

  test("no Pages menu or palette group without a declared page", async ({ page }) => {
    await mockApis(page, []);
    await searchPalette(page, "board");
    await expect(page.getByRole("option", { name: /Board/ })).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(page.getByRole("button", { name: "Pages" })).toHaveCount(0);
  });

  test("opens from the TopBar full width, sandboxed, and Back returns to the session", async ({ page }) => {
    await mockApis(page, [PAGE]);
    const order: string[] = [];
    page.on("request", (req) => {
      const path = new URL(req.url()).pathname;
      if (path.startsWith(PAGE.url)) order.push(`${req.method()} ${path}`);
    });
    const frameApiCalls: string[] = [];
    page.on("request", (req) => {
      if (req.frame() !== page.mainFrame() && req.url().includes("/api/sessions")) frameApiCalls.push(req.url());
    });

    await page.goto("/session/sess-a");
    await page.getByRole("button", { name: "Pages" }).click();
    await page.getByRole("menuitem", { name: "Board" }).click();

    await expect(page).toHaveURL("/plugins/acme.kit/board");
    await expect(page.getByRole("navigation", { name: "Sessions sidebar" })).toHaveCount(0);
    const frame = page.getByTestId("plugin-page-frame");
    await expect(frame).toHaveAttribute("sandbox", "allow-scripts");
    await expect(frame).toHaveAttribute("src", new RegExp(`^${PAGE.url}#[0-9a-f]{32}$`));
    await expect(page.frameLocator("[data-testid=plugin-page-frame]").getByText("board fixture")).toBeVisible();
    expect(order.slice(0, 2)).toEqual([`POST ${PAGE.url}/ticket`, `GET ${PAGE.url}`]);

    const report = await page.waitForFunction(
      () => (window as typeof window & { __frameReports: unknown[] }).__frameReports[0],
    );
    const { storage, cookie, api, href } = (await report.jsonValue()) as Record<string, string>;
    expect(storage).toMatch(/^blocked:/);
    expect(cookie).toMatch(/^blocked:/);
    expect(api).toMatch(/^blocked:/);
    expect(href).not.toContain("token");
    expect(frameApiCalls).toEqual([]);

    await page.getByRole("button", { name: "← Back" }).click();
    await expect(page).toHaveURL("/session/sess-a");
  });

  test("the palette lists the page; an unknown page shows an empty state", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await searchPalette(page, "board");
    await page.getByRole("option", { name: /Board/ }).click();
    await expect(page).toHaveURL("/plugins/acme.kit/board");
    await expect(page.getByTestId("plugin-page-frame")).toBeVisible();

    await page.goto("/plugins/acme.kit/missing");
    await expect(page.getByText("Page not found")).toBeVisible();
    // A deep link has no in-app history, so Back lands on the dashboard.
    await page.getByRole("button", { name: "← Back" }).click();
    await expect(page).toHaveURL("/");
  });

  test("the page round-trips every bridge method and follows theme changes without reloading", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await page.route(`**${PAGE.url}`, (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: BRIDGE_FIXTURE }),
    );
    await page.route("**/api/theme/current", (r) => r.fulfill({ json: theme("day", "light", "#ffffff") }));
    await page.route("**/api/themes/night", (r) => r.fulfill({ json: theme("night", "dark", "#000000") }));
    const actionBodies: unknown[] = [];
    await page.route("**/api/plugins/*/action", (r) => {
      actionBodies.push({ url: new URL(r.request().url()).pathname, body: r.request().postDataJSON() });
      return r.fulfill({ status: 202, json: { ok: true, baseline_revision: 3 } });
    });
    const frameLoads: string[] = [];
    page.on("request", (req) => {
      if (new URL(req.url()).pathname === PAGE.url && req.method() === "GET") frameLoads.push(req.url());
    });

    await page.goto("/plugins/acme.kit/board");
    const latest = (kind: string) =>
      page.evaluate(
        (k) =>
          (window as typeof window & { __frameReports: { kind?: string }[] }).__frameReports
            .filter((m) => m?.kind === k)
            .at(-1),
        kind,
      );
    await expect.poll(() => latest("results")).toBeTruthy();
    const results = (await latest("results")) as unknown as Record<string, Record<string, unknown>>;
    expect(results.ready).toEqual({ aoeBridge: 1, id: 1, result: null });
    expect(results.pushes).toEqual([
      {
        aoeBridge: 1,
        event: "context",
        data: { readOnly: false, cityhall: false, pluginId: "acme.kit", pageId: "board", bridgeVersion: 1 },
      },
      { aoeBridge: 1, event: "theme", data: { appearance: "light", cssVars: { "--color-surface-900": "#ffffff" } } },
    ]);
    expect((results.sessions.result as { id: string }[]).map((s) => s.id)).toEqual(["sess-a"]);
    expect(results.uiState.result).toEqual({ entries: [], notifications: [], pages: [PAGE] });
    expect(results.action).toEqual({ aoeBridge: 1, id: 4, result: { baselineRevision: 3 } });
    expect(actionBodies).toEqual([
      {
        url: "/api/plugins/acme.kit/action",
        body: { method: "move", params: { to: "done" }, session_id: "sess-a" },
      },
    ]);
    expect(results.unknown).toMatchObject({ id: 5, error: { code: "unknown_method" } });

    await page.evaluate(() =>
      window.dispatchEvent(new CustomEvent("aoe:theme-picker-changed", { detail: { name: "night" } })),
    );
    await expect
      .poll(() => latest("theme"))
      .toEqual({ kind: "theme", data: { appearance: "dark", cssVars: { "--color-surface-900": "#000000" } } });
    expect(frameLoads).toHaveLength(1);

    const frame = page.frames().find((f) => new URL(f.url()).pathname === PAGE.url);
    await frame!.evaluate(() =>
      (window as typeof window & { call: (m: string, p: unknown) => unknown }).call("openSession", { id: "sess-a" }),
    );
    await expect(page).toHaveURL("/session/sess-a");
  });

  test("a page another document replaces before ready never hands it the bridge", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await page.route(`**${PAGE.url}`, (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: REDIRECT_FIXTURE }),
    );
    await page.route("**/api/plugins/acme.other/pages/board", (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: OTHER_FIXTURE }),
    );
    const actions: string[] = [];
    await page.route("**/api/plugins/*/action", (r) => {
      actions.push(r.request().url());
      return r.fulfill({ status: 202, json: { ok: true } });
    });
    // The host answers an accepted ready synchronously while dispatching it, before the page's next message.
    await page.addInitScript(() => {
      const w = window as typeof window & { __bridgePosts: unknown[] };
      w.__bridgePosts = [];
      const post = MessagePort.prototype.postMessage;
      MessagePort.prototype.postMessage = function (this: MessagePort, msg: unknown, ...rest: never[]) {
        if ((msg as { aoeBridge?: number } | null)?.aoeBridge) w.__bridgePosts.push(msg);
        return post.call(this, msg, ...rest);
      };
    });

    await page.goto("/plugins/acme.kit/board");
    await page.waitForFunction(() =>
      (window as typeof window & { __frameReports: { kind?: string }[] }).__frameReports.some(
        (m) => m?.kind === "other-sent",
      ),
    );
    expect(await page.evaluate(() => (window as typeof window & { __bridgePosts: unknown[] }).__bridgePosts)).toEqual(
      [],
    );
    expect(actions).toEqual([]);
  });
});
