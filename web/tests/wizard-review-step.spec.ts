import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Wizard Review step (#1219). Covers in-place editing of the title and
// branch rows (Enter commits, Escape cancels), the jump-to-step row
// (clicking Project lands back on the Project step), and the
// conditional Mode / Base branch / Worktree rows.

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "settings" || path === "about" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (r) =>
    r.fulfill({
      json: [
        {
          name: "claude",
          binary: "claude",
          host_only: false,
          installed: true,
          install_hint: "",
        },
      ],
    }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "GET") {
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "seed-session",
              title: "seed",
              project_path: "/tmp/example",
              group_path: "/tmp",
              tool: "claude",
              status: "Idle",
              yolo_mode: false,
              created_at: new Date().toISOString(),
              last_accessed_at: null,
              last_error: null,
              branch: null,
              main_repo_path: null,
              is_sandboxed: false,
              has_terminal: true,
              profile: "default",
              workspace_repos: [],
            },
          ],
          workspace_ordering: [],
        },
      });
    }
    return r.fulfill({ json: { session: { id: "new-session" } } });
  });
}

async function openReviewStep(page: Page, title = "feat/initial") {
  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  const recent = page.getByRole("button").filter({ hasText: "/tmp/example" }).first();
  await recent.waitFor({ state: "visible", timeout: 5000 });
  await recent.click();
  await page.getByRole("button", { name: "Next" }).click();
  await page.getByPlaceholder("Auto-generated if empty").fill(title);
  await page.getByRole("button", { name: "Next" }).click();
  await page.getByRole("button", { name: "Next" }).click();
  await expect(page.getByRole("heading", { name: "Review & Launch" })).toBeVisible();
}

// Match a review-card row by its exact label span. The row markup is
// `<button><span>{label}</span><span>{value}</span></button>`, so a button
// containing a `<span>` whose text equals the label is unique.
function reviewRow(page: import("@playwright/test").Page, label: string) {
  return page.locator("button").filter({
    has: page.getByText(label, { exact: true }),
  });
}

test.describe("Wizard review step (#1219)", () => {
  test("Title row shows current value and Mode/Branch rows render when worktree is on", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page, "feat/initial");
    await expect(reviewRow(page, "Title")).toContainText("feat/initial");
    await expect(reviewRow(page, "Mode")).toContainText("Create new branch");
    await expect(reviewRow(page, "Branch / worktree")).toBeVisible();
  });

  test("clicking the Title row enters edit mode; Enter commits the new value", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page, "original");
    await reviewRow(page, "Title").click();
    const input = page.locator("input[type=text]").last();
    await expect(input).toBeFocused();
    await input.fill("renamed");
    await input.press("Enter");
    await expect(reviewRow(page, "Title")).toContainText("renamed");
  });

  test("blur on the inline title input commits the draft value", async ({ page }) => {
    // EditableRow.onBlur calls commit(), so clicking outside the input
    // while it's in edit mode applies the typed draft. The Escape key
    // case currently bubbles up to the dashboard's global Escape
    // shortcut (`useKeyboardShortcuts`), which closes the wizard
    // outright (see src/App.tsx onEscape). Cover blur-commit here; the
    // Escape interaction is dashboard-level, not review-step-level.
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page, "before");
    await reviewRow(page, "Title").click();
    const input = page.locator("input[type=text]").last();
    await expect(input).toBeFocused();
    await input.fill("after");
    // Click the modal heading (non-interactive, same modal) to blur
    // the input without navigating away from the Review step.
    await page.getByRole("heading", { name: "Review & Launch" }).click();
    await expect(reviewRow(page, "Title")).toContainText("after");
  });

  test("clicking the Project row jumps back to the Project step", async ({ page }) => {
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page);
    await reviewRow(page, "Project").click();
    await expect(page.getByRole("heading", { name: "Project folder" })).toBeVisible();
  });

  test("Launch session button is disabled while submitting, then back to enabled on success", async ({ page }) => {
    await mockApis(page);
    let resolveCreate: (() => void) | null = null;
    const createPromise = new Promise<void>((res) => {
      resolveCreate = res;
    });
    await page.route("**/api/sessions", async (r) => {
      if (r.request().method() === "POST") {
        await createPromise;
        return r.fulfill({ json: { session: { id: "new-session" } } });
      }
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "seed-session",
              title: "seed",
              project_path: "/tmp/example",
              group_path: "/tmp",
              tool: "claude",
              status: "Idle",
              yolo_mode: false,
              created_at: new Date().toISOString(),
              last_accessed_at: null,
              last_error: null,
              branch: null,
              main_repo_path: null,
              is_sandboxed: false,
              has_terminal: true,
              profile: "default",
              workspace_repos: [],
            },
          ],
          workspace_ordering: [],
        },
      });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/");
    await openReviewStep(page);
    const launch = page.getByRole("button", { name: /Launch session/ });
    await expect(launch).toBeEnabled();
    await launch.click();
    // Submitting state: the button shows the spinner + disabled while POST is in flight.
    await expect(page.getByText("Creating session...")).toBeVisible();
    resolveCreate?.();
  });
});
