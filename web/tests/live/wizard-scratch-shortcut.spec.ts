// User story: Cmd+Shift+N (Mac) / Ctrl+Shift+N (other) opens the
// wizard pre-configured for a scratch session AND jumped to the Review
// step, so a follow-up Cmd+Enter / Ctrl+Enter creates the session in
// two keystrokes total. Closes #1324.

import { basename, dirname } from "node:path";
import { test as base, expect } from "@playwright/test";
import { listSessions, spawnAoeServe } from "../helpers/aoeServe";

base("Cmd+Shift+N opens wizard at Review with scratch on; Cmd+Enter launches", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    // Wait for the dashboard surface to be interactive before firing
    // global shortcuts. The sidebar's "New session" button doubles as
    // a proxy for "the document-level keydown handler is registered."
    await expect(page.getByRole("button", { name: "New session", exact: true }).first()).toBeVisible({
      timeout: 15_000,
    });

    // Cross-platform modifier: Playwright's chromium reports
    // navigator.platform as "MacIntel" on macOS hosts and "Linux x86_64"
    // on CI. `useKeyboardShortcuts` derives IS_MAC the same way, so
    // mirror the check here. Use `ControlOrMeta` so we honor the host's
    // actual modifier without re-implementing the IS_MAC dance.
    await page.keyboard.press("ControlOrMeta+Shift+KeyN");

    const wizard = page.locator('div.fixed.inset-0.z-50:has(h1:has-text("New session"))');
    await expect(wizard).toBeVisible({ timeout: 10_000 });

    // The wizard must land on the Review step (skipToReview is set
    // alongside scratch by the App.tsx callback). Look for the Launch
    // button + the scratch project marker; if either is missing the
    // prefill plumbing regressed.
    await expect(wizard.getByRole("button", { name: /Launch session/ })).toBeVisible({ timeout: 10_000 });
    await expect(wizard.getByText(/Scratch directory \(provisioned on create\)/)).toBeVisible();

    // Second keystroke: Cmd+Enter / Ctrl+Enter on ReviewStep submits.
    // ReviewStep's effect listens for `metaKey || ctrlKey` so the same
    // ControlOrMeta combo covers both platforms.
    await page.keyboard.press("ControlOrMeta+Enter");

    // Server-side: a single scratch session lands.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);

    const sessions = await listSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    const session = sessions[0]!;
    expect(session.scratch).toBe(true);
    // node:path helpers handle Windows `\` separators as well as
    // POSIX `/`, so the assertion stays correct cross-platform.
    const projectPath = session.project_path as string;
    expect(basename(dirname(projectPath))).toBe("scratch");
  } finally {
    await serve.stop();
  }
});
