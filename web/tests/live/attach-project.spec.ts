// Attaching a repo to a session that already exists, over the daemon (#3103).
//
// The Vitest spec covers the sidebar payload and the modal's rendering of each
// outcome against a stubbed fetch. Nothing exercised the daemon end of it: the
// handler, `resolve_project_input`, the worktree that lands on disk, and the
// `worker` field the modal keys its notice off were all untested on any surface.
//
// Drives the live `aoe serve` backend over REST. Two repos are seeded before the
// server boots; the session is created on the first and the second is attached
// afterwards. No agent runs, so the worker outcome is deterministically
// `not_running` and the whole spec is timing-free.

import { test as base, expect } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { appDirFor, listSessions, resolveAoeBinary, spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";

const GIT_ENV = {
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@t",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@t",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_SYSTEM: "/dev/null",
} as const;

function run(cmd: string, args: string[], cwd: string) {
  const res = spawnSync(cmd, args, {
    cwd,
    env: { ...process.env, ...GIT_ENV },
    encoding: "utf8",
  });
  if (res.error || res.status !== 0) {
    const errMsg = res.error ? String(res.error) : "non-zero exit";
    throw new Error(
      `${cmd} ${args.join(" ")} failed in ${cwd}: ${errMsg}; status=${res.status}\nstdout=${res.stdout}\nstderr=${res.stderr}`,
    );
  }
  return res.stdout.trim();
}

function seedRepo(home: string, name: string, extraBranch?: string) {
  const dir = join(home, name);
  run("git", ["init", "-q", "--initial-branch=main", dir], home);
  writeFileSync(join(dir, "file.txt"), `${name}\n`);
  run("git", ["add", "file.txt"], dir);
  run("git", ["commit", "-q", "-m", "init"], dir);
  if (extraBranch) run("git", ["branch", extraBranch], dir);
  return dir;
}

interface AttachBody {
  project: string;
  attach_existing_branch?: boolean;
  restart?: boolean;
}

interface AttachResponse {
  attached: { name: string; branch: string; branch_created: boolean };
  warnings: string[];
  worker: string;
  worker_message: string | null;
}

async function attach(serve: ServeHandle, id: string, body: AttachBody) {
  const res = await fetch(`${serve.baseUrl}/api/sessions/${id}/projects`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return { status: res.status, text: await res.text() };
}

async function attachOk(serve: ServeHandle, id: string, body: AttachBody): Promise<AttachResponse> {
  const { status, text } = await attach(serve, id, body);
  if (status !== 200) throw new Error(`POST projects failed: ${status} ${text}`);
  return JSON.parse(text) as AttachResponse;
}

// Via `listSessions` so the `{ sessions: [...] }` envelope stays in one place.
async function sessionById(serve: ServeHandle, id: string) {
  const sessions = await listSessions(serve.baseUrl);
  const found = sessions.find((s) => s.id === id);
  if (!found) throw new Error(`session ${id} missing from /api/sessions`);
  return found as typeof found & { workspace_repos: { name: string; branch: string }[] };
}

base("attaching a project over the daemon lands a worktree and widens the session", async ({}, testInfo) => {
  let serve: ServeHandle | undefined;
  try {
    serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home }) => {
        seedRepo(home, "backend");
        // `taken` already has the branch the session will suggest, so the
        // refuse / opt-in behaviour can be exercised without a second attach.
        seedRepo(home, "frontend");
        seedRepo(home, "taken", "feature/attach-live");
      },
    });

    const created = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        path: join(serve.home, "backend"),
        tool: "claude",
        title: "attach-live",
        worktree_branch: "feature/attach-live",
        create_new_branch: true,
      }),
    });
    if (!created.ok) {
      throw new Error(`POST /api/sessions failed: ${created.status} ${await created.text()}`);
    }
    const { id } = (await created.json()) as { id: string };

    // An absolute path is accepted as-is by `resolve_project_input`. No agent is
    // running, so the daemon reports `not_running` rather than bouncing a worker,
    // which is the branch the modal turns into "will see this repo next start".
    const attached = await attachOk(serve, id, { project: join(serve.home, "frontend"), restart: true });
    expect(attached.attached.name).toBe("frontend");
    expect(attached.attached.branch).toBe("feature/attach-live");
    expect(attached.attached.branch_created).toBe(true);
    expect(attached.worker).toBe("not_running");

    // The worktree is real and on disk under the aoe-owned per-session dir. Via
    // `appDirFor` rather than a hardcoded path: a debug binary namespaces the app
    // dir as `-dev`, and macOS may resolve it under XDG or the legacy home.
    const appDir = appDirFor(serve.home, serve.env.XDG_CONFIG_HOME!, resolveAoeBinary());
    const worktree = join(appDir, "attached-repos", id, "frontend");
    expect(existsSync(worktree)).toBe(true);
    // Really a worktree of the repo, not just a directory.
    expect(existsSync(join(worktree, ".git"))).toBe(true);
    expect(run("git", ["rev-parse", "--abbrev-ref", "HEAD"], worktree)).toBe("feature/attach-live");

    // The DTO now describes both repos, which is what buckets the row into the
    // sidebar's Multi-repo group and labels each diff hunk.
    const widened = await sessionById(serve, id);
    expect(widened.workspace_repos.map((r) => r.name)).toEqual(["backend", "frontend"]);

    // Attaching the same repo twice is refused, by resolved main repo path.
    const dupe = await attach(serve, id, { project: join(serve.home, "frontend") });
    expect(dupe.status).toBe(400);
    expect(dupe.text).toContain("already attached");

    // A branch that already exists in the repo being attached is refused, since
    // a same-named branch there can hold unrelated commits.
    const takenPath = join(serve.home, "taken");
    const refused = await attach(serve, id, { project: takenPath });
    expect(refused.status).toBe(400);
    expect(refused.text).toContain("already exists");

    // Opting in checks it out and records that aoe did not create it, so session
    // deletion leaves the branch alone.
    const reused = await attachOk(serve, id, { project: takenPath, attach_existing_branch: true });
    expect(reused.attached.name).toBe("taken");
    expect(reused.attached.branch_created).toBe(false);

    // A path that is not a repo is rejected rather than silently accepted.
    const notARepo = await attach(serve, id, { project: join(serve.home, "nope") });
    expect(notARepo.status).toBe(400);

    // A bare name that is not in the project registry is rejected too, which is
    // the other half of `resolve_project_input`.
    const unknownName = await attach(serve, id, { project: "not-registered" });
    expect(unknownName.status).toBe(400);
  } finally {
    await serve?.stop();
  }
});
