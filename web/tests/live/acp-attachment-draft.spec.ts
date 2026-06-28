// Structured view composer attachment-draft persistence (#2493).
//
// Staged composer images must survive a session switch and a page reload
// the same way unsent text drafts already do. Before the fix, switching
// sessions remounted AcpRuntime (`<StructuredView key={sessionId}>`),
// dropping the in-memory `pendingAttachments`, so a pasted/picked image
// vanished even though its text draft came back.
//
// This drives the real composer: stage an image via the file picker,
// switch sessions through the command palette (client-side remount),
// switch back, and assert the chip is restored; then reload and assert it
// survives that too. It also asserts session B does not inherit A's draft.

import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

// A valid 1x1 PNG (same fixture as acp-attachment.spec.ts).
const PNG_1X1_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

function seedTwoSessions(): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  return ({ home, env }) => {
    for (const [title, subdir] of [
      ["draft-source", "project-a"],
      ["draft-target", "project-b"],
    ] as const) {
      const projectDir = join(home, subdir);
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
      const res = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", title, "-c", "claude"], { env });
      if (res.status !== 0) {
        throw new Error(`aoe add ${title} failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
      }
    }
  };
}

base("staged composer image survives session switch and reload", async ({ page }, testInfo) => {
  // Advertise the image prompt capability so the composer enables attachments.
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-acp-draft-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify({ promptCapabilities: { image: true } }));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTwoSessions(),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const source = sessions.find((s) => s.title === "draft-source")!;
    const target = sessions.find((s) => s.title === "draft-target")!;

    await enableStructuredViewAndWait(serve.baseUrl, source.id, 30_000, serve.home);
    await enableStructuredViewAndWait(serve.baseUrl, target.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(source.id)}`);
    await waitForStructuredView(page);

    // Attachments only stage once the agent's image capability has arrived,
    // which flips the attach button out of its "Waiting for agent
    // capabilities" disabled state.
    const attachButton = page.getByRole("button", { name: /Attach files/ });
    await expect(attachButton).toBeEnabled({ timeout: 30_000 });

    // Stage an image through the composer's hidden file input.
    await page.locator('input[type="file"]').setInputFiles({
      name: "shot.png",
      mimeType: "image/png",
      buffer: Buffer.from(PNG_1X1_B64, "base64"),
    });

    const chip = page.getByRole("img", { name: "shot.png" });
    await expect(chip).toBeVisible({ timeout: 10_000 });

    // Switch to session B via the command palette (client-side remount).
    await page.keyboard.press(`${MOD}+K`);
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await expect(palette).toBeVisible({ timeout: 5_000 });
    await palette.getByPlaceholder("Search actions, sessions, settings…").fill("draft-target");
    await palette.getByText("draft-target").first().click();
    await expect(page).toHaveURL(new RegExp(`/session/${target.id}`), { timeout: 10_000 });
    await waitForStructuredView(page);

    // B must not inherit A's staged attachment.
    await expect(page.getByRole("img", { name: "shot.png" })).toHaveCount(0);

    // Switch back to A: its staged image must be restored.
    await page.keyboard.press(`${MOD}+K`);
    await expect(palette).toBeVisible({ timeout: 5_000 });
    await palette.getByPlaceholder("Search actions, sessions, settings…").fill("draft-source");
    await palette.getByText("draft-source").first().click();
    await expect(page).toHaveURL(new RegExp(`/session/${source.id}`), { timeout: 10_000 });
    await waitForStructuredView(page);
    await expect(page.getByRole("img", { name: "shot.png" })).toBeVisible({ timeout: 10_000 });

    // And it survives a full page reload.
    await page.reload();
    await waitForStructuredView(page);
    await expect(page.getByRole("img", { name: "shot.png" })).toBeVisible({ timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
