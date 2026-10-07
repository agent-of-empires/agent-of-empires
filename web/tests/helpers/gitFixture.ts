// Git repositories for live specs.

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const GIT_ENV = {
  // Ignore the developer's global git config (gpgsign, hooksPath); identity and branch are pinned.
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_SYSTEM: "/dev/null",
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@t",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@t",
};

/** Preserve the fixture environment, never the invoking shell's Git overrides. */
export function gitEnv(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const clean: NodeJS.ProcessEnv = {};
  for (const [name, value] of Object.entries(env)) {
    if (!name.startsWith("GIT_")) clean[name] = value;
  }
  return { ...clean, ...GIT_ENV };
}

export interface BareRepoFixture {
  path: string;
  url: string;
}

/** An empty bare repo under an existing parent, clonable via its `file://` URL. */
export function createBareRepo(parentDir: string, env: NodeJS.ProcessEnv, name = "bare.git"): BareRepoFixture {
  const path = join(parentDir, name);
  mkdirSync(parentDir, { recursive: true });
  const res = spawnSync("git", ["init", "--bare", "--quiet", path], {
    env: gitEnv(env),
  });
  if (res.status !== 0) {
    throw new Error(`git init --bare failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
  }
  return { path, url: `file://${path}` };
}

function runGit(cwd: string, args: string[], env: NodeJS.ProcessEnv): void {
  const res = spawnSync("git", args, {
    cwd,
    env: gitEnv(env),
  });
  if (res.status !== 0) {
    throw new Error(
      `git ${args.join(" ")} failed (cwd=${cwd}): status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
    );
  }
}

/** A working repo on `defaultBranch` with one empty commit to diff against. */
export function initWorkingRepo(
  repoPath: string,
  env: NodeJS.ProcessEnv,
  opts: { defaultBranch?: string } = {},
): { path: string } {
  const branch = opts.defaultBranch ?? "main";
  mkdirSync(repoPath, { recursive: true });
  runGit(repoPath, ["init", "-q", "-b", branch], env);
  runGit(repoPath, ["commit", "--allow-empty", "-q", "-m", "init"], env);
  return { path: repoPath };
}

/** A bare repo with one commit, so a bare clone has a branch to check out as a worktree. */
export function createSeededBareRepo(
  parentDir: string,
  env: NodeJS.ProcessEnv,
  opts: { name?: string; defaultBranch?: string } = {},
): BareRepoFixture {
  const name = opts.name ?? "seeded-bare.git";
  const branch = opts.defaultBranch ?? "main";
  mkdirSync(parentDir, { recursive: true });
  const path = join(parentDir, name);
  const workdir = join(parentDir, `${name}.src`);
  initWorkingRepo(workdir, env, { defaultBranch: branch });
  const res = spawnSync("git", ["clone", "--bare", "--quiet", workdir, path], {
    env: gitEnv(env),
  });
  if (res.status !== 0) {
    throw new Error(`git clone --bare failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
  }
  return { path, url: `file://${path}` };
}

/** Write files (creating directories) without committing. */
export function writeFiles(repoPath: string, files: Record<string, string>): void {
  for (const [relPath, content] of Object.entries(files)) {
    const abs = join(repoPath, relPath);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, content);
  }
}

export function writeBinaryFile(repoPath: string, relPath: string, bytes: Uint8Array): void {
  const abs = join(repoPath, relPath);
  mkdirSync(dirname(abs), { recursive: true });
  writeFileSync(abs, bytes);
}

export function commitAll(repoPath: string, message: string, env: NodeJS.ProcessEnv): void {
  runGit(repoPath, ["add", "-A"], env);
  runGit(repoPath, ["commit", "-q", "-m", message], env);
}

/** `lineCount` unique lines, so a specific mid-file line can be located. */
export function generateLargeFileContent(lineCount: number, prefix = "line"): string {
  const lines = new Array<string>(lineCount);
  for (let i = 0; i < lineCount; i++) {
    lines[i] = `${prefix} ${i}: ${"lorem ".repeat(8).trim()}`;
  }
  return lines.join("\n") + "\n";
}

/** PNG signature bytes: enough for git to classify the file as binary. */
export function pngStubBytes(): Uint8Array {
  return new Uint8Array([
    0x89,
    0x50,
    0x4e,
    0x47,
    0x0d,
    0x0a,
    0x1a,
    0x0a, // PNG signature
    0x00,
    0x00,
    0x00,
    0x0d,
    0x49,
    0x48,
    0x44,
    0x52, // IHDR length + type
    0x00,
    0x00,
    0x00,
    0x01,
    0x00,
    0x00,
    0x00,
    0x01, // 1x1
    0x08,
    0x06,
    0x00,
    0x00,
    0x00,
    0x1f,
    0x15,
    0xc4,
    0x89, // bit-depth, color-type, crc
    0x00,
    0x00,
    0x00,
    0x00,
    0x49,
    0x45,
    0x4e,
    0x44, // IEND length + type
    0xae,
    0x42,
    0x60,
    0x82, // IEND crc
  ]);
}
