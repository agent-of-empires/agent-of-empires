// Live coverage for the sidebar sort picker (#1418, #1640).
//
// Drives a real `aoe serve` subprocess against three seeded sessions
// whose `created_at` timestamps differ by ~1.2s each. Asserts that
// selecting "Last activity" reorders the sidebar rows by newest-created
// first, that selecting "Attention" is reachable and persists, and that
// the localStorage-backed preference survives a page reload.
//
// Pure comparator semantics (status ranks, urgent promotion, null
// fallback) are covered by the Vitest suite at
// web/src/lib/__tests__/sidebarSort.test.ts, and status-differentiated
// Attention ordering by the mocked Playwright suite at
// web/tests/sidebar-sort-mode.spec.ts (live sessions all share a status,
// so the live spec proves wiring, not the status comparator). The mocked
// suite also covers drag disablement and the multi-repo pin. This spec
// proves the wiring boots against the real server end-to-end.
//
// Pairs with web/tests/live/workspace-ordering.spec.ts for manual-mode
// regressions.
//
// The seeded `created_at` deltas use 1.2s sleeps. RFC3339 timestamps
// the server emits have sub-second precision, so a tighter gap would
// still work, but the wider window keeps this resilient to any process
// scheduling jitter between sequential `aoe add` invocations.

import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../helpers/aoeServe";

interface SeededSession {
  dir: string;
  title: string;
}

function seedSequentialSessions(sessions: SeededSession[]) {
  return async ({
    home,
    env,
  }: {
    home: string;
    shimBin: string;
    env: NodeJS.ProcessEnv;
  }) => {
    const binary = resolveAoeBinary();
    for (let i = 0; i < sessions.length; i++) {
      const { dir, title } = sessions[i]!;
      const projectDir = join(home, dir);
      mkdirSync(projectDir, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: projectDir });
      spawnSync(
        "git",
        ["commit", "--allow-empty", "-q", "-m", "init"],
        {
          cwd: projectDir,
          env: {
            ...env,
            GIT_AUTHOR_NAME: "t",
            GIT_AUTHOR_EMAIL: "t@t",
            GIT_COMMITTER_NAME: "t",
            GIT_COMMITTER_EMAIL: "t@t",
          },
        },
      );
      const res = spawnSync(
        binary,
        ["add", projectDir, "-t", title, "-c", "claude"],
        { env },
      );
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed for ${title}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
      if (i < sessions.length - 1) {
        await new Promise((r) => setTimeout(r, 1200));
      }
    }
  };
}

async function readWorkspaceTitles(page: import("@playwright/test").Page) {
  return page.evaluate(() => {
    const rows = Array.from(
      document.querySelectorAll<HTMLAnchorElement>(
        "[data-testid='sidebar-session-row']",
      ),
    );
    return rows
      .map((a) => a.querySelector("[title]")?.getAttribute("title") ?? "")
      .filter(Boolean);
  });
}

const TOGGLE = "[data-testid='sidebar-sort-toggle']";

// The control is a dropdown picker: open it, then click the labeled option.
async function selectSortMode(
  page: import("@playwright/test").Page,
  mode: string,
) {
  await page.locator(TOGGLE).click();
  await page.locator(`[data-testid='sidebar-sort-option-${mode}']`).click();
  await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort-mode", mode);
}

base.describe("sidebar sort picker live (#1418, #1640)", () => {
  base(
    "picker reorders by newest-created, reaches attention, and persists across reload",
    async ({ page }, testInfo) => {
      const serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: seedSequentialSessions([
          { dir: "repo-oldest", title: "oldest-session" },
          { dir: "repo-middle", title: "middle-session" },
          { dir: "repo-newest", title: "newest-session" },
        ]),
      });

      try {
        const seeded = await listSessions(serve.baseUrl);
        expect(seeded).toHaveLength(3);

        await page.goto(`${serve.baseUrl}/`);

        // 4-worker cold start can lag past Playwright's 5s default;
        // bump the first paint waits, matching sidebar-groups.spec.ts.
        const rows = page.locator("[data-testid='sidebar-session-row']");
        await expect(rows).toHaveCount(3, { timeout: 10_000 });
        await expect(page.locator(TOGGLE)).toHaveAttribute(
          "data-sort-mode",
          "manual",
        );

        // Capture the manual-mode order. With three brand-new sessions
        // and no prior drag, the workspace_ordering file is prepended
        // newest-first as workspaces are observed (see #1171 server
        // merge), so the live default already happens to match the
        // last-activity outcome. We assert against the toggle's effect,
        // not against a specific manual ordering, by capturing it.
        const manualOrder = await readWorkspaceTitles(page);
        expect(manualOrder.sort()).toEqual([
          "middle-session",
          "newest-session",
          "oldest-session",
        ]);

        await selectSortMode(page, "lastActivity");

        await expect
          .poll(() => readWorkspaceTitles(page), { timeout: 5000 })
          .toEqual([
            "newest-session",
            "middle-session",
            "oldest-session",
          ]);

        // Reload: localStorage carries the picker state across reloads
        // even against the live server.
        await page.reload();
        await expect(rows).toHaveCount(3, { timeout: 10_000 });
        await expect(page.locator(TOGGLE)).toHaveAttribute(
          "data-sort-mode",
          "lastActivity",
        );
        await expect
          .poll(() => readWorkspaceTitles(page), { timeout: 5000 })
          .toEqual([
            "newest-session",
            "middle-session",
            "oldest-session",
          ]);

        // Attention is reachable end-to-end and persists. All three live
        // sessions share a status, so the rendered order is not asserted
        // here (the mocked suite covers status-differentiated ordering);
        // this proves the third mode boots against the real server.
        await selectSortMode(page, "attention");
        await expect(rows).toHaveCount(3, { timeout: 5000 });
        const storedAttention = await page.evaluate(() =>
          window.localStorage.getItem("aoe-sidebar-sort-mode"),
        );
        expect(storedAttention).toBe("attention");

        // Back to manual: returns to whatever the server has as manual
        // order. We don't assert a specific order here because the
        // server's workspace-ordering merge produces an order that's
        // valid but depends on observation timing; the contract we
        // care about is "selecting manual exits the computed modes."
        await selectSortMode(page, "manual");
      } finally {
        await serve.stop();
      }
    },
  );
});
