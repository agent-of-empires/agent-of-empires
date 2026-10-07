import { spawnSync } from "node:child_process";
import { join } from "node:path";
import type { Locator, Page } from "@playwright/test";
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

/** Open the mobile sidebar only if it is closed, then wait for the slide-in to settle. */
export async function openMobileSidebar(page: Page) {
  const toggle = page.getByRole("button", { name: "Toggle sidebar" });
  await toggle.waitFor({ state: "visible", timeout: 10_000 });
  const probe = page.getByTestId("sidebar-session-row").first();
  await probe.waitFor({ state: "attached", timeout: 10_000 });
  const initial = await probe.boundingBox();
  if (!initial || initial.x < 0) await toggle.click();
  await page.waitForFunction(
    () => {
      const r = document.querySelector('[data-testid="sidebar-session-row"]')?.getBoundingClientRect();
      return !!r && r.x >= 0 && r.width > 0;
    },
    null,
    { timeout: 5_000 },
  );
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
