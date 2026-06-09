// Live-backend spec: right panel's "non-diff" surfaces (#1221).
//
// Two tests live here:
//   1) Paired-terminal toggle on a non-structured view session. The lower half
//      of `RightPanel.tsx` exposes a Host / Container shell mode picker
//      (`src/components/RightPanel.tsx:373-397`). Non-sandboxed sessions
//      must show Host only; Container is gated on `is_sandboxed`. This
//      asserts the toggle exists and that the paired-terminal pane
//      mounts.
//   2) Comments-banner notification flow on a structured view session. Once a
//      user stages a comment by selecting a diff gutter line, the
//      `CommentsBanner` (`src/components/diff/comments/CommentsBanner.tsx`)
//      surfaces a notification chip in the right panel with the comment
//      count, plus Send and Discard-all actions. This exercises the
//      end-to-end staging + dismissal path.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { listSessions, resolveAoeBinary, seedSessionViaAoeAdd, spawnAoeServe } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

base(
  "right panel paired terminal: Host shown, Container hidden on non-sandboxed session",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "rp-paired" }),
    });

    try {
      await page.goto(`${serve.baseUrl}/`);
      const sessionRow = page.getByRole("link").filter({ hasText: "rp-paired" }).first();
      await expect(sessionRow).toBeVisible({ timeout: 10_000 });
      await sessionRow.click();

      // Paired-terminal pane lives in the lower half of RightPanel. The
      // "Shell" label sits above the Host / Container picker; assert it
      // first to scope subsequent selectors to that pane. The dashboard
      // mounts both a desktop and a mobile right panel (one hidden via
      // CSS), so use first() on visible-anywhere assertions.
      await expect(page.getByText("Shell", { exact: true }).first()).toBeVisible({
        timeout: 10_000,
      });
      // Host button is rendered unconditionally; Container only when
      // `is_sandboxed`. The seeded session is not sandboxed, so no
      // Container button should exist in either copy of the panel.
      await expect(page.getByRole("button", { name: "Host", exact: true }).first()).toBeVisible();
      await expect(page.getByRole("button", { name: "Container", exact: true })).toHaveCount(0);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "right panel notifications: structured view comments banner appears on stage, clears on discard",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home, env }) => {
        const projectDir = join(home, "project");
        initWorkingRepo(projectDir);
        // Commit a baseline so the modified version produces a real
        // hunk with selectable gutter line numbers (the comments UI
        // requires line numbers on at least one side).
        writeFiles(projectDir, { "notes.md": "line a\nline b\nline c\n" });
        commitAll(projectDir, "baseline");
        writeFiles(projectDir, { "notes.md": "line A\nline B\nline C\n" });
        const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "rp-notif", "-c", "claude"], { env });
        if (addRes.status !== 0) {
          throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
        }
      },
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId = sessions.find((s) => s.title === "rp-notif")?.id;
      if (!sessionId) {
        throw new Error("seeded structured view session not visible in /api/sessions");
      }

      // Flip per-session structured_view so the SPA renders the comments
      // affordances on the diff viewer. Same pattern as
      // acp-spawn-prompt.spec.ts; the supervisor spawn is async, so
      // give it a beat before driving the UI.
      const enableRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });
      expect(enableRes.ok).toBeTruthy();
      await new Promise((r) => setTimeout(r, 2_000));

      // Browser auto-accepts the window.confirm fired by Discard-all.
      page.on("dialog", (dialog) => {
        void dialog.accept();
      });

      await page.goto(`${serve.baseUrl}/`);
      const sessionRow = page.getByRole("link").filter({ hasText: "rp-notif" }).first();
      await expect(sessionRow).toBeVisible({ timeout: 10_000 });
      await sessionRow.click();

      // Wait for the file list to populate; one modified file expected.
      // first() picks the desktop right-panel copy (the dashboard also
      // mounts a mobile copy hidden via CSS).
      await expect(page.getByText("1 file", { exact: true }).first()).toBeVisible({
        timeout: 15_000,
      });
      await page
        .getByRole("button", { name: /notes\.md/ })
        .first()
        .click();

      // Select a line to comment by clicking its @pierre/diffs gutter line
      // number. The renderer exposes `[data-line-number-content]` cells that
      // contain only the number; a single click selects the line and opens
      // the inline comment form.
      const gutterLine1 = page.locator("[data-line-number-content]").filter({ hasText: /^1$/ });
      await expect(gutterLine1.first()).toBeVisible({ timeout: 10_000 });
      await gutterLine1.first().click();

      // Form textarea autofocuses; type a body and save.
      const textarea = page.getByPlaceholder(/Leave a comment/);
      await expect(textarea).toBeFocused({ timeout: 5_000 });
      await textarea.fill("nit");
      await page.getByRole("button", { name: "Save", exact: true }).click();

      // CommentsBanner now lives in the right panel showing the count
      // and the Send / Discard-all actions.
      await expect(page.getByText("1 comment", { exact: true }).first()).toBeVisible({ timeout: 10_000 });
      await expect(page.getByRole("button", { name: "Send", exact: true }).first()).toBeVisible();
      await expect(page.getByRole("button", { name: "Discard all", exact: true }).first()).toBeVisible();

      // Discard-all confirms via window.confirm (auto-accepted above)
      // and clears the store, removing the banner from both panel copies.
      await page.getByRole("button", { name: "Discard all", exact: true }).first().click();
      await expect(page.getByText("1 comment", { exact: true })).toHaveCount(0, { timeout: 10_000 });
    } finally {
      await serve.stop();
    }
  },
);
