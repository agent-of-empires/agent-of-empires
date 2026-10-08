import type { Page, Route } from "@playwright/test";
import { test, expect, publishedRequests } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";
import { sessionResponse } from "./helpers/sessions";
import { readVisibleSessionTitles } from "./helpers/sidebar";

const headers = (revision: bigint, epoch = "boot") => ({
  "aoe-runtime-epoch": epoch,
  "aoe-runtime-revision": revision.toString(),
});
async function bodyCount(page: Page, path: string) {
  return page.evaluate(
    (path) => (window as typeof window & { __mockedBodies: string[] }).__mockedBodies.filter((p) => p === path).length,
    path,
  );
}
async function consume(page: Page, route: Route, json: unknown, revision: bigint, epoch = "boot") {
  const path = new URL(route.request().url()).pathname;
  const before = await bodyCount(page, path);
  await route.fulfill({ headers: headers(revision, epoch), json });
  await expect.poll(() => bodyCount(page, path)).toBeGreaterThan(before);
}
async function polls(page: Page) {
  const held: Route[] = [];
  await page.route("**/api/sessions", (r) => {
    held.push(r);
  });
  return async () => {
    await expect.poll(() => held.length, { timeout: 10_000 }).toBeGreaterThan(0);
    return held.shift()!;
  };
}
const initialA = sessionResponse({
  id: "A",
  title: "alpha",
  project_path: "/tmp/repo",
  branch: "feature/a",
  color: null,
  notify_on_waiting: null,
  notify_on_idle: null,
  notify_on_error: null,
});
const B = sessionResponse({ id: "B", title: "beta", project_path: "/tmp/repo", branch: "feature/b", color: null });
async function boot(page: Page) {
  await installSidebarMocks(page, {
    sessions: [
      { id: "A", title: "alpha", project_path: "/tmp/repo", branch: "feature/a" },
      { id: "B", title: "beta", project_path: "/tmp/repo", branch: "feature/b" },
    ],
  });
  const next = await polls(page);
  await page.goto("/");
  return next;
}
const row = (page: Page, title: string) => page.getByTestId("sidebar-session-row").filter({ hasText: title });
async function menu(page: Page, title: string) {
  await page.getByRole("button", { name: "Go to dashboard" }).click();
  await row(page, title).click({ button: "right" });
}

for (const setting of ["color", "notifications"] as const) {
  test(`${setting} cancellation installs a held lower receipt after another row ACK and accepts the next canonical writer`, async ({
    page,
  }) => {
    const next = await boot(page);
    const base = 9007199254740992n;
    const selectedA =
      setting === "color"
        ? { ...initialA, color: "red" }
        : { ...initialA, notify_on_waiting: true, notify_on_idle: true, notify_on_error: true };
    const selectedB = { ...B, color: "green" };
    const list = (a = initialA, b = B) => ({ sessions: [a, b], workspace_ordering: [] });
    await consume(page, await next(), list(), base + 9n);
    await expect(row(page, "alpha")).toBeVisible();
    const pending: Route[] = [];
    const path = "/api/sessions/A/" + setting;
    await page.route("**" + path, (r) => {
      pending.push(r);
    });
    await page.route("**/api/sessions/B/color", (r) => r.fulfill({ headers: headers(base + 12n), json: selectedB }));
    const earlier = setting === "color" ? "sidebar-context-menu-color-red" : "sidebar-context-menu-notify-all";
    const latest = setting === "color" ? "sidebar-context-menu-color-clear" : "sidebar-context-menu-notify-default";
    await menu(page, "alpha");
    await page.getByTestId(earlier).click();
    await expect.poll(() => pending.length).toBe(1);
    await page.getByTestId(latest).click();
    await expect(page.getByTestId(latest)).toHaveAttribute("aria-pressed", "true");
    await consume(page, await next(), list(selectedA), base + 10n);
    await expect(page.getByTestId(latest)).toHaveAttribute("aria-pressed", "true");
    expect((await publishedRequests(page, path, "PATCH")).length).toBe(1);
    await menu(page, "beta");
    await page.getByTestId("sidebar-context-menu-color-green").click();
    await expect(row(page, "beta").getByTestId("sidebar-session-color-dot")).toHaveAttribute("data-color", "green");
    await menu(page, "alpha");
    await expect(page.getByTestId(latest)).toHaveAttribute("aria-pressed", "true");
    const first = pending.shift()!;
    expect(first.request().postDataJSON()).toEqual(
      setting === "color" ? { color: "red" } : { notify_on_waiting: true, notify_on_idle: true, notify_on_error: true },
    );
    await consume(page, first, selectedA, base + 10n);
    await expect.poll(() => pending.length).toBe(1);
    const cancel = pending.shift()!;
    expect(cancel.request().postDataJSON()).toEqual(
      setting === "color" ? { color: null } : { notify_on_waiting: null, notify_on_idle: null, notify_on_error: null },
    );
    await consume(page, cancel, initialA, base + 11n);
    await expect(page.getByTestId(latest)).toHaveAttribute("aria-pressed", "true");
    await consume(page, await next(), list(selectedA, selectedB), base + 11n);
    await expect(page.getByTestId(latest)).toHaveAttribute("aria-pressed", "true");
    if (setting === "color") await expect(row(page, "alpha").getByTestId("sidebar-session-color-dot")).toHaveCount(0);
    await consume(page, await next(), list(selectedA, selectedB), base + 13n);
    await expect(page.getByTestId(earlier)).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId(latest)).toHaveAttribute("aria-pressed", "false");
    if (setting === "color")
      await expect(row(page, "alpha").getByTestId("sidebar-session-color-dot")).toHaveAttribute("data-color", "red");
  });
}

for (const staleKind of ["same epoch", "abandoned epoch"] as const) {
  test(`valid stale ${staleKind} polls restore online state without admitting stale rows or settings`, async ({
    page,
  }) => {
    const next = await boot(page);
    const order = ["/tmp/repo::feature/a", "/tmp/repo::feature/b"];
    const good = { sessions: [initialA, B], workspace_ordering: order };
    await consume(page, await next(), good, 1n, "old-boot");
    await consume(page, await next(), good, 10n, "current-boot");
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["alpha", "beta"]);
    await (await next()).fulfill({ status: 503 });
    const offline = page.locator('[title="Disconnected from backend"]');
    await expect(offline).toBeVisible();
    const poisoned = {
      ...initialA,
      title: "stale-title",
      color: "red",
      notify_on_waiting: true,
      notify_on_idle: true,
      notify_on_error: true,
    };
    await consume(
      page,
      await next(),
      { sessions: [poisoned, B], workspace_ordering: [...order].reverse() },
      staleKind === "same epoch" ? 9n : 999n,
      staleKind === "same epoch" ? "current-boot" : "old-boot",
    );
    await expect(offline).toHaveCount(0);
    await expect(row(page, "alpha")).toBeVisible();
    await expect(row(page, "beta")).toBeVisible();
    await expect(row(page, "stale-title")).toHaveCount(0);
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["alpha", "beta"]);
    await menu(page, "alpha");
    await expect(page.getByTestId("sidebar-context-menu-color-clear")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("sidebar-context-menu-notify-default")).toHaveAttribute("aria-pressed", "true");
    await page.keyboard.press("Escape");
    await consume(
      page,
      await next(),
      { sessions: [{ ...poisoned, title: "canonical-next" }, B], workspace_ordering: [...order].reverse() },
      11n,
      "current-boot",
    );
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["beta", "canonical-next"]);
    await menu(page, "canonical-next");
    await expect(page.getByTestId("sidebar-context-menu-color-red")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("sidebar-context-menu-notify-all")).toHaveAttribute("aria-pressed", "true");
  });
}

test("a hung sessions poll aborts at its deadline and the next canonical poll restores online controls", async ({
  page,
}) => {
  test.setTimeout(45_000);
  const next = await boot(page);
  const good = { sessions: [initialA, B], workspace_ordering: [] };
  await consume(page, await next(), good, 1n);
  const hung = await next();
  const failed = page.waitForEvent("requestfailed", {
    predicate: (request) => request.url() === hung.request().url() && request.method() === "GET",
    timeout: 20_000,
  });
  await failed;
  await expect(page.locator('[title="Disconnected from backend"]')).toBeVisible();
  await expect(row(page, "alpha")).toBeVisible();
  await consume(
    page,
    await next(),
    { sessions: [{ ...initialA, title: "recovered-after-deadline" }, B], workspace_ordering: [] },
    2n,
  );
  await expect(page.locator('[title="Disconnected from backend"]')).toHaveCount(0);
  await expect(row(page, "recovered-after-deadline")).toBeVisible();
  await menu(page, "recovered-after-deadline");
  await expect(page.getByTestId("sidebar-context-menu-color-red")).toBeEnabled();
});
