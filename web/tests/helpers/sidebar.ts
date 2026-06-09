import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { resolveAoeBinary } from "./aoeServe";

export async function clickSidebarSession(page: Page, title: string) {
  // Sidebar session rows are anchor tags (see WorkspaceSidebar.tsx). The
  // pre-link button-based fallback that used to live here was dead code by
  // the time it ran: if the link never appeared, the button locator never
  // did either, but its click had no timeout and would consume the rest
  // of the 30s test budget before erroring with a confusing "page closed"
  // message. Surfacing the link timeout directly gives a clean failure.
  //
  // 20s rather than 10s: at 6 mocked workers on a 4-vCPU runner with v8
  // coverage instrumentation, sidebar slide-in + sessions fetch + render
  // sometimes lose to scheduler contention past 10s. Two flakes in CI
  // (pinch-zoom MIN_FONT_SIZE, scrollback scroll-down clamp) traced to
  // exactly this race.
  const sessionLink = page.getByRole("link").filter({ hasText: title }).first();
  await sessionLink.waitFor({ state: "visible", timeout: 20_000 });
  // On mobile the sidebar slides in from the left; while it is closed or
  // mid-transition, the row's bounding box has a negative x. Playwright's
  // `visible` check passes (non-zero box, not display:none) but `.click()`
  // then loops on "element is outside of the viewport" for the full 30s
  // test timeout. Wait until the box settles inside the viewport.
  await page.waitForFunction(
    (linkTitle) => {
      const links = Array.from(document.querySelectorAll("a"));
      const link = links.find((a) => a.textContent?.includes(linkTitle));
      if (!link) return false;
      const r = link.getBoundingClientRect();
      return r.x >= 0 && r.y >= 0 && r.width > 0 && r.height > 0;
    },
    title,
    { timeout: 10_000 },
  );
  await sessionLink.click();
}

// Mobile specs (`devices['iPhone 13']`) open the workspace sidebar before
// clicking a session row. The old recipe `if (await toggle.isVisible())`
// is a single non-retrying snapshot that races with React hydration on
// loaded CI workers, so the toggle click is skipped and the sidebar stays
// closed; the subsequent row click then times out on actionability.
//
// This helper is deterministic: it waits for the toggle to mount, probes
// the sidebar's current x via the session-row testid, and only clicks the
// toggle when the sidebar is fully closed (x < -row width / 2). It then
// blocks until the slide-in transition settles the box at x >= 0.
export async function openMobileSidebar(page: Page) {
  const toggle = page.getByRole("button", { name: "Toggle sidebar" });
  await toggle.waitFor({ state: "visible", timeout: 10_000 });
  const probe = page.getByTestId("sidebar-session-row").first();
  await probe.waitFor({ state: "attached", timeout: 10_000 });
  const initial = await probe.boundingBox();
  if (!initial || initial.x < 0) {
    await toggle.click();
  }
  await page.waitForFunction(
    () => {
      const row = document.querySelector('[data-testid="sidebar-session-row"]');
      if (!row) return false;
      const r = (row as HTMLElement).getBoundingClientRect();
      return r.x >= 0 && r.width > 0;
    },
    null,
    { timeout: 5_000 },
  );
}

/**
 * Read the visible session-row titles in the sidebar, in DOM order.
 *
 * Scopes to the `.truncate` label span inside each row so a Wakeup or
 * Plan chip on the same row can't be confused with the workspace
 * title. Hoisted from `tests/live/workspace-ordering.spec.ts` so the
 * reorder story specs can share the same accessor without redefining
 * it per file.
 */
export async function readVisibleSessionTitles(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const rows = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='sidebar-session-row']"));
    return rows.map((r) => r.querySelector("span.truncate[title]")?.getAttribute("title") ?? "").filter(Boolean);
  });
}

/**
 * Build a `seedFn` for `spawnAoeServe` that git-inits a project dir
 * under the isolated HOME, then runs `aoe add` once per supplied
 * title. The arrival order matters: the server prepends new workspace
 * ids newest-first, so the seeded sidebar order in arrival sequence
 * is `titles[-1]` at the top down to `titles[0]` at the bottom.
 *
 * Optional `subdir` lets callers seed multiple repos by pointing each
 * `seedSessionsInRepo` call at a different directory; the cross-group
 * reorder story chains two calls for that reason.
 */
export function seedSessionsInRepo(opts: {
  titles: string[];
  subdir?: string;
  tool?: string;
}): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  return ({ home, env }) => {
    const binary = resolveAoeBinary();
    const projectDir = join(home, opts.subdir ?? "repo");
    mkdirSync(projectDir, { recursive: true });
    spawnSync("git", ["init", "-q"], { cwd: projectDir });
    spawnSync("git", ["commit", "--allow-empty", "-q", "-m", "init"], {
      cwd: projectDir,
      env: {
        ...env,
        GIT_AUTHOR_NAME: "t",
        GIT_AUTHOR_EMAIL: "t@t",
        GIT_COMMITTER_NAME: "t",
        GIT_COMMITTER_EMAIL: "t@t",
      },
    });
    for (const title of opts.titles) {
      const res = spawnSync(binary, ["add", projectDir, "-t", title, "-c", opts.tool ?? "claude"], { env });
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed for ${title}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

/**
 * Chain multiple repo seeds into a single `seedFn`. Cross-group
 * reorder stories need at least two repos so dnd-kit's per-group
 * SortableContext renders multiple drag boundaries; this helper runs
 * each repo's `seedSessionsInRepo` in sequence under the same env.
 */
export function seedRepos(
  repos: Array<{ titles: string[]; subdir: string; tool?: string }>,
): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  const fns = repos.map((r) => seedSessionsInRepo(r));
  return (seedEnv) => {
    for (const fn of fns) fn(seedEnv);
  };
}
