// #4460: plugin pages open full width from the TopBar or palette, framed with
// `sandbox="allow-scripts allow-downloads"` so plugin script never runs on the dashboard origin.

import { readFileSync } from "node:fs";
import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis, mockSessionShellApis } from "./helpers/apiMocks";
import { mockSessionsList, sessionResponse } from "./helpers/sessions";
import type { Page } from "@playwright/test";

const PAGE = {
  plugin_id: "acme.kit",
  id: "board",
  title: "Board",
  icon: "kanban",
  url: "/api/plugins/acme.kit/pages/board",
  capabilities: ["session.read"],
};

const WRITE_PAGE = { ...PAGE, capabilities: ["session.read", "session.write"] };

// The daemon's page policy (src/server/api/plugins.rs `PLUGIN_PAGE_CSP`).
const PAGE_CSP =
  "sandbox allow-scripts allow-downloads; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; " +
  "img-src data: blob:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

// Reports what the page could reach back to the dashboard.
const FIXTURE = `<!doctype html><p>board fixture</p><button id="export">Export</button><script>
document.getElementById("export").onclick = () => {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob(['{"ok":true}'], { type: "application/json" }));
  a.download = "board.json";
  a.click();
};
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
  const archive = await call("session.archive", { id: "sess-a" });
  const trash = await call("session.trash", { id: "sess-a" });
  parent.postMessage({ kind: "results", ready, sessions, uiState, action, unknown, archive, trash, pushes: pushes.slice(0, 2) }, "*");
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
  await mockSessionsList(page, () => [{ id: "sess-a", title: "Ethiopians", project_path: "/tmp/agent-of-empires" }]);
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: [], notifications: [], pages } }));
  await page.route(`**${PAGE.url}/ticket`, (r) => r.fulfill({ status: 204 }));
  await page.route(`**${PAGE.url}`, (r) =>
    r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: FIXTURE }),
  );
}

/** Opens the palette once the first ui-state poll has landed, so a missing entry is not just an early read.
 *  Starts on a session, where no first-run overlay covers the palette. */
/** Records archive and trash requests, each answered with the session row. */
async function mockLifecycle(page: Page): Promise<string[]> {
  const calls: string[] = [];
  await page.route(/\/api\/sessions\/sess-a\/(archive|trash)$/, (r) => {
    const req = r.request();
    calls.push(`${req.method()} ${new URL(req.url()).pathname} ${req.postData()}`);
    return r.fulfill({ json: sessionResponse({ id: "sess-a", title: "Ethiopians" }) });
  });
  return calls;
}

const latestReport = (page: Page, kind: string) =>
  page.evaluate(
    (k) =>
      (window as typeof window & { __frameReports: { kind?: string }[] }).__frameReports
        .filter((m) => m?.kind === k)
        .at(-1),
    kind,
  );

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

  test("no page button or palette group without a declared page", async ({ page }) => {
    await mockApis(page, []);
    await searchPalette(page, "board");
    await expect(page.getByRole("option", { name: /Board/ })).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(page.getByRole("banner").getByRole("button", { name: "Board", exact: true })).toHaveCount(0);
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
    await page.getByRole("banner").getByRole("button", { name: "Board", exact: true }).click();

    await expect(page).toHaveURL("/plugins/acme.kit/board");
    await expect(page.getByRole("banner").getByRole("button", { name: "Board", exact: true })).toHaveAttribute(
      "aria-current",
      "page",
    );
    await expect(page.getByRole("navigation", { name: "Sessions sidebar" })).toHaveCount(0);
    const frame = page.getByTestId("plugin-page-frame");
    await expect(frame).toHaveAttribute("sandbox", "allow-scripts allow-downloads");
    await expect(frame).toHaveAttribute("src", new RegExp(`^${PAGE.url}#[0-9a-f]{32}$`));
    await expect(page.frameLocator("[data-testid=plugin-page-frame]").getByText("board fixture")).toBeVisible();
    const download = page.waitForEvent("download");
    await page.frameLocator("[data-testid=plugin-page-frame]").getByRole("button", { name: "Export" }).click();
    const saved = await download;
    expect(saved.suggestedFilename()).toBe("board.json");
    expect(readFileSync((await saved.path())!, "utf8")).toBe('{"ok":true}');
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
    await mockApis(page, [WRITE_PAGE]);
    await page.route(`**${PAGE.url}`, (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: BRIDGE_FIXTURE }),
    );
    const lifecycle = await mockLifecycle(page);
    await page.route("**/api/theme/current", (r) => r.fulfill({ json: theme("day", "light", "#ffffff") }));
    await page.route("**/api/themes/night", (r) => r.fulfill({ json: theme("night", "dark", "#000000") }));
    // The worker-shaped rows, not the wider /api/sessions ones.
    const summary = { id: "sess-a", title: "Ethiopians", project_path: "/tmp/agent-of-empires", status: "Idle" };
    await page.route("**/api/plugins/acme.kit/sessions", (r) => r.fulfill({ json: { sessions: [summary] } }));
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
    const latest = (kind: string) => latestReport(page, kind);
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
    expect(results.sessions.result).toEqual([summary]);
    expect(results.uiState.result).toEqual({ entries: [], notifications: [], pages: [WRITE_PAGE] });
    expect(results.action).toEqual({ aoeBridge: 1, id: 4, result: { baselineRevision: 3 } });
    expect(actionBodies).toEqual([
      {
        url: "/api/plugins/acme.kit/action",
        body: { method: "move", params: { to: "done" }, session_id: "sess-a" },
      },
    ]);
    expect(results.unknown).toMatchObject({ id: 5, error: { code: "unknown_method" } });
    expect(results.archive).toEqual({ aoeBridge: 1, id: 6, result: null });
    expect(results.trash).toEqual({ aoeBridge: 1, id: 7, result: null });
    expect(lifecycle).toEqual([
      'PATCH /api/sessions/sess-a/archive {"archived":true,"kill_pane":true}',
      'POST /api/sessions/sess-a/trash {"kill_pane":true}',
    ]);

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

  test("split view shows the opened session below the page and tells the page", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await page.route(`**${PAGE.url}`, (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: BRIDGE_FIXTURE }),
    );
    await page.route("**/api/plugins/*/action", (r) => r.fulfill({ status: 202, json: { ok: true } }));

    await page.goto("/plugins/acme.kit/board");
    await expect.poll(() => latestReport(page, "results")).toBeTruthy();
    const frame = page.frames().find((f) => new URL(f.url()).pathname === PAGE.url)!;
    const selections = () => frame.evaluate("pushes.filter((m) => m.event === 'selection').map((m) => m.data)");
    await expect.poll(selections).toEqual([{ sessionId: null }]);

    await page.getByRole("button", { name: "Split view" }).click();
    await expect(page.getByRole("button", { name: "Split view" })).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("plugin-page-split-session")).toContainText("No session selected");

    await frame.evaluate("call('openSession', { id: 'sess-a' })");
    const pane = page.getByTestId("plugin-page-split-session");
    await expect(pane.getByTestId("plugin-page-split-title")).toHaveText("Ethiopians");
    // The pane is the session's full view, docks included.
    await expect(pane.getByTestId("pane-tab-diff")).toBeVisible();
    await expect(page).toHaveURL("/plugins/acme.kit/board?session=sess-a");
    await expect.poll(selections).toEqual([{ sessionId: null }, { sessionId: "sess-a" }]);

    // Maximized, the page hides without reloading.
    await page.getByRole("button", { name: "Maximize session" }).click();
    await expect(page.getByTestId("plugin-page-frame")).toBeHidden();
    await page.getByRole("button", { name: "Restore split" }).click();
    await expect(page.getByTestId("plugin-page-frame")).toBeVisible();
    expect(await frame.evaluate("pushes.filter((m) => m.event === 'selection').length")).toBe(2);

    await page.getByRole("button", { name: "Open full" }).click();
    await expect(page).toHaveURL("/session/sess-a");
  });

  test("session.contextMenu opens the sidebar's session menu over the page", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await page.route(`**${PAGE.url}`, (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: BRIDGE_FIXTURE }),
    );
    await page.route("**/api/plugins/*/action", (r) => r.fulfill({ status: 202, json: { ok: true } }));
    await page.route("**/api/plugins/acme.kit/sessions", (r) => r.fulfill({ json: { sessions: [{ id: "sess-a" }] } }));
    const renames: unknown[] = [];
    await page.route("**/api/sessions/sess-a", (r) => {
      if (r.request().method() !== "PATCH") return r.fallback();
      renames.push(r.request().postDataJSON());
      return r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "2" },
        json: {
          ...sessionResponse({ id: "sess-a", title: "Renamed", project_path: "/tmp/agent-of-empires" }),
          outcome: { warnings: [] },
        },
      });
    });

    await page.goto("/plugins/acme.kit/board");
    await expect.poll(() => latestReport(page, "results")).toBeTruthy();
    const frame = page.frames().find((f) => new URL(f.url()).pathname === PAGE.url)!;
    const open = (items: unknown[] = []) =>
      frame.evaluate((items) => {
        // Not returned: evaluate would await the reply, which comes only once the menu closes.
        const w = window as typeof window & { menu: unknown; call: (m: string, p: unknown) => Promise<unknown> };
        w.menu = undefined;
        void w.call("session.contextMenu", { id: "sess-a", x: 40, y: 50, items }).then((reply) => (w.menu = reply));
      }, items);
    const menuReply = () => frame.evaluate(() => (window as typeof window & { menu: unknown }).menu);
    const menu = page.getByTestId("sidebar-context-menu");

    await open([{ id: "move:done", label: "Move to Done" }]);
    await expect(menu).toBeVisible();
    const frameBox = (await page.getByTestId("plugin-page-frame").boundingBox())!;
    const menuBox = (await menu.boundingBox())!;
    expect(menuBox.x).toBeCloseTo(frameBox.x + 40, 0);
    expect(menuBox.y).toBeCloseTo(frameBox.y + 50, 0);
    await menu.getByRole("button", { name: "Move to Done" }).click();
    await expect(menu).toHaveCount(0);
    await expect.poll(menuReply).toMatchObject({ result: "move:done" });

    await open();
    await page.getByTestId("sidebar-context-menu-snooze").click();
    await expect(page.getByTestId("snooze-modal")).toBeVisible();
    await expect.poll(menuReply).toMatchObject({ result: null });
    await page.keyboard.press("Escape");

    await open();
    await page.getByTestId("sidebar-context-menu-rename").click();
    await page.getByTestId("rename-modal-input").fill("Renamed");
    await page.getByRole("button", { name: "Rename", exact: true }).click();
    await expect(page.getByTestId("rename-modal")).toHaveCount(0);
    expect(renames).toEqual([{ title: "Renamed" }]);

    // A click back on the page dismisses the menu.
    await open();
    await expect(menu).toBeVisible();
    // A raw click does not retry, so wait out the shield that swallows clicks just after opening.
    await expect(page.getByTestId("plugin-session-menu-backdrop")).toHaveAttribute("data-armed", "true");
    await page.mouse.click(frameBox.x + frameBox.width - 20, frameBox.y + frameBox.height - 20);
    await expect(menu).toHaveCount(0);
  });

  test("a page without session.write cannot archive or trash", async ({ page }) => {
    await mockApis(page, [PAGE]);
    await page.route(`**${PAGE.url}`, (r) =>
      r.fulfill({ contentType: "text/html", headers: { "content-security-policy": PAGE_CSP }, body: BRIDGE_FIXTURE }),
    );
    await page.route("**/api/plugins/*/action", (r) => r.fulfill({ status: 202, json: { ok: true } }));
    const lifecycle = await mockLifecycle(page);

    await page.goto("/plugins/acme.kit/board");
    await expect.poll(() => latestReport(page, "results")).toBeTruthy();
    const results = (await latestReport(page, "results")) as unknown as Record<string, unknown>;
    expect(results.archive).toMatchObject({ id: 6, error: { code: "capability_denied" } });
    expect(results.trash).toMatchObject({ id: 7, error: { code: "capability_denied" } });
    expect(lifecycle).toEqual([]);
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
