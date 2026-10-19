// #4627: the document capture shortcut must not unmount Settings before a
// dialog's own Escape handler can run in the browser.
import { test, expect, publishedRequests } from "./helpers/mockedTest";
import { mockSettingsApis, mockStaticApis } from "./helpers/apiMocks";

const EXAMPLE = {
  id: "example.plugin",
  name: "Example",
  version: "1.0.0",
  description: "A community plugin.",
  enabled: true,
  builtin: false,
  validation: "community",
  source: "gh:example/plugin",
  capabilities: [],
  ui_contributions: [],
  granted: true,
  needs_reapproval: false,
};

test.beforeEach(async ({ page }) => {
  await mockStaticApis(page);
  await mockSettingsApis(page);
  await page.route("**/api/plugins", (route) =>
    route.fulfill({
      json: {
        plugins: [EXAMPLE, { ...EXAMPLE, id: "aoe.web", name: "Web dashboard", builtin: true, source: null }],
        load_errors: [],
      },
    }),
  );
  await page.route("**/api/plugins/details?*", (route) =>
    route.fulfill({
      json: {
        source: EXAMPLE.source,
        manifest: {
          ...EXAMPLE,
          api_version: 4,
          screenshots: [{ src: "/plugin-preview.svg", alt: "Plugin preview", caption: "" }],
        },
        manifest_error: null,
        release_tags: [],
      },
    }),
  );
  await page.route("**/plugin-preview.svg", (route) =>
    route.fulfill({
      contentType: "image/svg+xml",
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><rect width="160" height="90" fill="teal"/></svg>',
    }),
  );
  await page.goto("/settings/plugins");
  await expect(page.getByTestId("plugin-example.plugin")).toBeVisible();
});

test("Escape dismisses uninstall without uninstalling or leaving Settings", async ({ page }) => {
  await page.getByTestId("plugin-uninstall-example.plugin").click();
  const dialog = page.getByRole("dialog", { name: "Uninstall Example", exact: true });
  await expect(dialog).toBeVisible();

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(page).toHaveURL(/\/settings\/plugins$/);
  await expect(page.getByTestId("plugin-example.plugin")).toBeVisible();
  expect(await publishedRequests(page, "/api/plugins/example.plugin/uninstall", "POST")).toEqual([]);

  await page.keyboard.press("Escape");
  await expect(page).toHaveURL(/\/$/);
});

test("Escape closes only the lightbox, then details, then Settings", async ({ page }) => {
  await page.getByTestId("plugin-open-example.plugin").click();
  const details = page.getByRole("dialog", { name: "Example details", exact: true });
  await expect(details).toBeVisible();
  await page.getByRole("button", { name: "View Plugin preview full size" }).click();
  const lightbox = page.getByRole("dialog", { name: "Plugin preview full size", exact: true });
  await expect(lightbox).toBeVisible();

  await page.keyboard.press("Escape");
  await expect(lightbox).toBeHidden();
  await expect(details).toBeVisible();
  await expect(page).toHaveURL(/\/settings\/plugins$/);

  await page.keyboard.press("Escape");
  await expect(details).toBeHidden();
  await expect(page).toHaveURL(/\/settings\/plugins$/);

  await page.keyboard.press("Escape");
  await expect(page).toHaveURL(/\/$/);
});

test("Escape cancels disabling the Web dashboard and keeps it enabled", async ({ page }) => {
  const toggle = page.getByRole("switch", { name: "Enable Web dashboard", exact: true });
  await toggle.click();
  const dialog = page.getByRole("dialog", { name: "Turn off the Web dashboard?", exact: true });
  await expect(dialog).toBeVisible();
  await expect(page.getByTestId("web-dashboard-off-confirm")).toBeFocused();

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(page).toHaveURL(/\/settings\/plugins$/);
  await expect(toggle).toBeChecked();
  expect(await publishedRequests(page, "/api/plugins/aoe.web/enabled", "POST")).toEqual([]);
});

test("Escape still closes the global About dialog without leaving Settings", async ({ page }) => {
  await page.getByRole("button", { name: "More options", exact: true }).click();
  await page.getByRole("menuitem", { name: "About", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Agent of Empires", exact: true });
  await expect(dialog).toBeVisible();

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(page).toHaveURL(/\/settings\/plugins$/);
});

test("Escape closes the palette before the underlying uninstall confirmation", async ({ page }) => {
  await page.getByTestId("plugin-uninstall-example.plugin").click();
  const uninstall = page.getByRole("dialog", { name: "Uninstall Example", exact: true });
  await expect(uninstall).toBeVisible();
  await page.keyboard.press("ControlOrMeta+k");
  const palette = page.getByRole("dialog", { name: "Command palette", exact: true });
  await expect(palette).toBeVisible();
  await expect(palette.getByRole("combobox")).toBeFocused();

  await page.keyboard.press("Escape");
  await expect(palette).toBeHidden();
  await expect(uninstall).toBeVisible();
  await expect(page).toHaveURL(/\/settings\/plugins$/);
  expect(await publishedRequests(page, "/api/plugins/example.plugin/uninstall", "POST")).toEqual([]);

  await page.keyboard.press("Escape");
  await expect(uninstall).toBeHidden();
  await expect(page).toHaveURL(/\/settings\/plugins$/);
  expect(await publishedRequests(page, "/api/plugins/example.plugin/uninstall", "POST")).toEqual([]);

  await page.keyboard.press("Escape");
  await expect(page).toHaveURL(/\/$/);
});

for (const terminal of ["succeeded", "failed", "gone"] as const) {
  test(`Escape keeps a running plugin job open and dismisses it when ${terminal}`, async ({ page }) => {
    let state: "running" | typeof terminal = "running";
    let pluginReloads = 0;
    page.on("request", (request) => {
      if (request.method() === "GET" && new URL(request.url()).pathname === "/api/plugins") pluginReloads++;
    });
    await page.route("**/api/plugins/example.plugin/uninstall", (route) =>
      route.fulfill({ json: { job_id: "example-job" } }),
    );
    await page.route("**/api/plugins/jobs/example-job?*", (route) =>
      state === "gone"
        ? route.fulfill({ status: 404, json: { message: "Job no longer available." } })
        : route.fulfill({
            json: {
              job: {
                id: "example-job",
                kind: "uninstall",
                target: EXAMPLE.id,
                status: state === "failed" ? { state, error: "Example job failed." } : { state },
                started_at: 1,
                finished_at: state === "running" ? null : 2,
              },
              log: { exists: true, tail: "Example job log", lines_returned: 1, truncated: false },
            },
          }),
    );
    await page.getByTestId("plugin-uninstall-example.plugin").click();
    await page.getByTestId("plugin-uninstall-confirm-button").click();
    const job = page.getByTestId("plugin-job-modal");
    await expect(job).toBeVisible();
    await expect(page.getByTestId("plugin-job-log")).toHaveText("Example job log");
    await expect(page.getByTestId("plugin-job-close")).toBeDisabled();
    await page.keyboard.press("Escape");
    await expect(job).toBeVisible();
    await expect(page).toHaveURL(/\/settings\/plugins$/);
    expect(pluginReloads).toBe(0);

    state = terminal;
    await expect(page.getByTestId("plugin-job-close")).toBeEnabled();
    if (terminal === "succeeded") {
      await page.keyboard.press("ControlOrMeta+k");
      const palette = page.getByRole("dialog", { name: "Command palette", exact: true });
      await expect(palette).toBeVisible();
      await expect(palette.getByRole("combobox")).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(palette).toBeHidden();
      await expect(job).toBeVisible();
      expect(pluginReloads).toBe(0);
    }
    await page.keyboard.press("Escape");
    await expect(job).toBeHidden();
    await expect(page).toHaveURL(/\/settings\/plugins$/);
    await expect.poll(() => pluginReloads).toBe(1);
    expect(await publishedRequests(page, "/api/plugins/example.plugin/uninstall", "POST")).toHaveLength(1);
    await page.keyboard.press("Escape");
    await expect(page).toHaveURL(/\/$/);
  });
}
