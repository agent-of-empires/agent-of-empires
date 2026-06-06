// User story: switch between sessions using the command palette.
//
// Open the palette via Cmd/Ctrl+K, type part of the target session's
// title to filter, click the result. The URL navigates to that
// session.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

function seedTwoSessions(): (seedEnv: {
  home: string;
  shimBin: string;
  env: NodeJS.ProcessEnv;
}) => void {
  return ({ home, env }) => {
    for (const [title, subdir] of [
      ["palette-source", "project-a"],
      ["palette-target", "project-b"],
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
      const res = spawnSync(
        resolveAoeBinary(),
        ["add", projectDir, "-t", title, "-c", "claude"],
        { env },
      );
      if (res.status !== 0) {
        throw new Error(
          `aoe add ${title} failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

base("command palette switches sessions", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTwoSessions(),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const source = sessions.find((s) => s.title === "palette-source")!;
    const target = sessions.find((s) => s.title === "palette-target")!;

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(source.id)}`,
    );
    await expect(page).toHaveURL(new RegExp(`/session/${source.id}`), {
      timeout: 10_000,
    });

    await page.keyboard.press(`${MOD}+K`);
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await expect(palette).toBeVisible({ timeout: 5_000 });
    await palette
      .getByPlaceholder("Search actions, sessions, settings…")
      .fill("palette-target");

    await palette.getByText("palette-target").first().click();
    await expect(page).toHaveURL(new RegExp(`/session/${target.id}`), {
      timeout: 10_000,
    });
  } finally {
    await serve.stop();
  }
});
