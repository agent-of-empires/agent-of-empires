import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { expect, type Locator, type Page } from "@playwright/test";
import { initWorkingRepo } from "./gitFixture";
import { resolveAoeBinary } from "./aoeServe";

/** Click a sidebar session link once it has slid fully into the viewport. */
export async function clickSidebarSession(page: Page, title: string) {
  const sessionLink = page.getByRole("link").filter({ hasText: title }).first();
  // Generous: coverage-instrumented parallel workers can take over 10s to render the sidebar.
  await sessionLink.waitFor({ state: "visible", timeout: 20_000 });
  // A sliding mobile sidebar reports a visible box at negative x, which click() cannot reach.
  await page.waitForFunction(
    (linkTitle) => {
      const link = Array.from(document.querySelectorAll("a")).find((a) => a.textContent?.includes(linkTitle));
      const r = link?.getBoundingClientRect();
      return !!r && r.x >= 0 && r.y >= 0 && r.width > 0 && r.height > 0;
    },
    title,
    { timeout: 10_000 },
  );
  await sessionLink.click();
}

/**
 * Press the desktop sidebar's resize handle and move through each `x` (pixels from the viewport's left edge,
 * where the desktop sidebar starts) in one gesture, releasing at the last.
 */
export async function dragSidebarHandle(page: Page, ...xs: number[]) {
  const handle = page.getByTestId("sidebar-resize-handle");
  await handle.waitFor({ state: "visible", timeout: 10_000 });
  const box = (await handle.boundingBox())!;
  const y = box.y + box.height / 2;
  await page.mouse.move(box.x + box.width / 2, y);
  await page.mouse.down();
  for (const x of xs) await page.mouse.move(x, y, { steps: 6 });
  await page.mouse.up();
}

/** Show the phone's session list: the home screen, or with a Session open the drawer ☰ opens. */
export async function openMobileSidebar(page: Page) {
  const menu = page.getByTestId("topbar-menu");
  const row = page.getByTestId("sidebar-session-row").first();
  await expect.poll(async () => (await menu.isVisible()) || (await row.isVisible()), { timeout: 10_000 }).toBe(true);
  if (await menu.isVisible()) await menu.click();
  await expect(row).toBeVisible({ timeout: 10_000 });
  // The drawer slides out from under the Session card; wait for the card to clear the row.
  await expect.poll(async () => (await row.boundingBox())?.x ?? -1).toBeGreaterThanOrEqual(0);
}

/** Session row titles in DOM order, read from the label span so row chips are ignored. */
export async function readVisibleSessionTitles(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const rows = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='sidebar-session-row']"));
    return rows.map((r) => r.querySelector("span.truncate[title]")?.getAttribute("title") ?? "").filter(Boolean);
  });
}

/** Press near the right edge of `source`, hold past dnd-kit's activation delay, and drop on `target`. */
export async function dragRow(page: Page, source: Locator, target: Locator, { release = true } = {}) {
  const sourceBox = await source.boundingBox();
  const targetBox = await target.boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row box missing");
  await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(250);
  await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });
  if (release) await page.mouse.up();
}

/** A live `seedFn` that registers one repo with an `aoe add` per title; later titles sort first. */
export function seedSessionsInRepo(opts: {
  titles: string[];
  subdir?: string;
  tool?: string;
}): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  return ({ home, env }) => {
    const projectDir = join(home, opts.subdir ?? "repo");
    initWorkingRepo(projectDir, env);
    for (const title of opts.titles) {
      const res = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", title, "-c", opts.tool ?? "claude"], { env });
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed for ${title}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}
