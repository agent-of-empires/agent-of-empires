// User story: composer draft persists across a session switch.
//
// Seeds two acp-enabled sessions A and B. Types a draft into A,
// navigates to B (the StructuredView for A unmounts), then back to A and
// asserts the draft text re-seeded from
// `acp:draft:<sessionId-A>` localStorage.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../../helpers/aoeServe";
import {
  enableStructuredViewAndWait,
  waitForStructuredView,
} from "../../helpers/acp";

function seedTwoSessions(): (seedEnv: {
  home: string;
  shimBin: string;
  env: NodeJS.ProcessEnv;
}) => void {
  return ({ home, env }) => {
    for (const [title, subdir] of [
      ["story-switch-a", "project-a"],
      ["story-switch-b", "project-b"],
    ] as const) {
      const projectDir = join(home, subdir);
      mkdirSync(projectDir, { recursive: true });
      const initRes = spawnSync("git", ["init", "-q"], {
        cwd: projectDir,
        env,
      });
      if (initRes.status !== 0) {
        throw new Error(
          `git init failed for ${title}: status=${initRes.status} stderr=${initRes.stderr?.toString() ?? "<none>"}`,
        );
      }
      const commitRes = spawnSync(
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
      if (commitRes.status !== 0) {
        throw new Error(
          `git commit failed for ${title}: status=${commitRes.status} stderr=${commitRes.stderr?.toString() ?? "<none>"}`,
        );
      }
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

base("composer draft survives a session switch", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTwoSessions(),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionA = sessions.find((s) => s.title === "story-switch-a");
    const sessionB = sessions.find((s) => s.title === "story-switch-b");
    if (!sessionA || !sessionB) {
      throw new Error(
        "seeded sessions 'story-switch-a' and/or 'story-switch-b' missing",
      );
    }

    for (const id of [sessionA.id, sessionB.id]) {
      await enableStructuredViewAndWait(serve.baseUrl, id);
    }

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionA.id)}`,
    );
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("draft-on-session-a");
    // Wait for the debounced localStorage write to land before navigating.
    await expect
      .poll(
        async () =>
          await page.evaluate(
            (id) => localStorage.getItem(`acp:draft:${id}`),
            sessionA.id,
          ),
        { timeout: 5_000 },
      )
      .toBe("draft-on-session-a");

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionB.id)}`,
    );
    await waitForStructuredView(page);
    // Session B starts with an empty composer.
    await expect(
      page.getByRole("textbox", { name: /Send a message/i }),
    ).toHaveValue("");

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionA.id)}`,
    );
    await waitForStructuredView(page);
    await expect(
      page.getByRole("textbox", { name: /Send a message/i }),
    ).toHaveValue("draft-on-session-a", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
