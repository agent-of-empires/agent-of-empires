// Large-diff handling. The diff is computed server-side and shipped as a
// unified patch; the client parses it as text (no diff algorithm on the main
// thread) and offloads highlighting to the worker pool, so even a big
// lockfile churn (+10k/-13k) renders without hanging the tab.
import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { makeAllDifferentPatch } from "./helpers/patch";
import { mockTerminalApis } from "./helpers/terminal-mocks";

// Realistic lockfile-ish lines (long, distinct), entirely different old vs new.
function lockLines(n: number, salt: string): string {
  return (
    Array.from(
      { length: n },
      (_, i) =>
        `  /@scope/pkg-${salt}-${i}@${(i % 9) + 1}.${i % 20}.${i % 7}: ` +
        `resolution: {integrity: sha512-${salt}${"abc123".repeat(8)}${i}}`,
    ).join("\n") + "\n"
  );
}

async function mount(page: Page, adds: number, dels: number) {
  // Precompute fixture data so none of this lands inside timed sections.
  const oldContent = lockLines(dels, "old");
  const newContent = lockLines(adds, "new");
  const patch = makeAllDifferentPatch("pnpm-lock.yaml", oldContent, newContent);
  await mockTerminalApis(page);
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({
      json: {
        files: [
          {
            path: "pnpm-lock.yaml",
            old_path: null,
            status: "modified",
            additions: adds,
            deletions: dels,
          },
        ],
        per_repo_bases: [{ base_branch: "main" }],
        warning: null,
      },
    }),
  );
  await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (r) =>
    r.fulfill({
      json: {
        file: {
          path: "pnpm-lock.yaml",
          old_path: null,
          status: "modified",
          additions: adds,
          deletions: dels,
        },
        old_content: oldContent,
        new_content: newContent,
        patch,
        is_binary: false,
        truncated: false,
      },
    }),
  );
  await page.goto("/");
  await clickSidebarSession(page, "pinch-test");
}

test.describe("Large diff handling", () => {
  test("a +10k/-13k lockfile churn renders without crashing", async ({
    page,
  }) => {
    await mount(page, 10000, 13000); // ~23k changed lines
    const t0 = Date.now();
    await page.getByText("pnpm-lock.yaml").first().click();
    await expect(page.locator("diffs-container").first()).toBeVisible({
      timeout: 30000,
    });
    // Content from the top of the diff is on screen.
    await expect(
      page.getByText("pkg-old-0@", { exact: false }).first(),
    ).toBeVisible({ timeout: 15000 });
    const elapsed = Date.now() - t0;
    console.log(`large lockfile render: ${elapsed}ms`);
    // Text-parse + virtualized render; the old contents-diffing path took ~8s
    // and could OOM the tab. Generous bound to avoid CI flake.
    expect(elapsed).toBeLessThan(10_000);
  });

  test("mid-size diff renders", async ({ page }) => {
    await mount(page, 4000, 4000);
    await page.getByText("pnpm-lock.yaml").first().click();
    await expect(page.locator("diffs-container").first()).toBeVisible({
      timeout: 30000,
    });
  });
});
