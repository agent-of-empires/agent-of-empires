// Git fixture helpers for live Playwright specs.
//
// Used by:
//   - `tests/live/git-clone.spec.ts`: spins up a throwaway bare repo so
//     the wizard can clone from `file://`. The matching server-side
//     validator accepts `file://` URLs by design; see
//     `src/server/api/git.rs::looks_like_git_url` for the allowlist.
//   - `tests/live/right-panel-*.spec.ts`: builds a non-bare working repo
//     with a committed `main` branch plus uncommitted modifications, so
//     `aoe add` registers the dir as a session and the diff endpoint
//     surfaces those modifications against the base branch.

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const GIT_ENV = {
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@t",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@t",
};

export interface BareRepoFixture {
  /** Absolute path of the bare repo on disk. */
  path: string;
  /** `file://` URL pointing at `path`, ready to feed into the wizard input. */
  url: string;
}

/**
 * Create a throwaway local bare git repo so a live `aoe serve` can clone
 * from `file://...`. Parent dir must already exist (the harness home tree
 * is created before this helper runs).
 *
 * Returns the absolute path and the `file://` URL.
 */
export function createBareRepo(parentDir: string, name = "bare.git"): BareRepoFixture {
  const path = join(parentDir, name);
  mkdirSync(parentDir, { recursive: true });
  const res = spawnSync("git", ["init", "--bare", "--quiet", path], {
    env: { ...process.env, ...GIT_ENV },
  });
  if (res.status !== 0) {
    throw new Error(`git init --bare failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
  }
  return { path, url: `file://${path}` };
}

function runGit(cwd: string, args: string[]): void {
  const res = spawnSync("git", args, {
    cwd,
    env: { ...process.env, ...GIT_ENV },
  });
  if (res.status !== 0) {
    throw new Error(
      `git ${args.join(" ")} failed (cwd=${cwd}): status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
    );
  }
}

/**
 * Initialize a non-bare working repo at `repoPath` on `defaultBranch`
 * with an initial empty commit so subsequent diffs have a base to
 * compare against. Uses `-b <branch>` so the default branch is
 * deterministic across hosts where `init.defaultBranch` may be unset.
 */
export function initWorkingRepo(repoPath: string, opts: { defaultBranch?: string } = {}): { path: string } {
  const branch = opts.defaultBranch ?? "main";
  mkdirSync(repoPath, { recursive: true });
  runGit(repoPath, ["init", "-q", "-b", branch]);
  runGit(repoPath, ["commit", "--allow-empty", "-q", "-m", "init"]);
  return { path: repoPath };
}

/**
 * Write a set of files into a repo (uncommitted). Paths are joined onto
 * `repoPath`; nested directories are created automatically. Use to
 * stage uncommitted modifications visible to the diff endpoint, or as
 * the source for a follow-up `commitAll`.
 */
export function writeFiles(repoPath: string, files: Record<string, string>): void {
  for (const [relPath, content] of Object.entries(files)) {
    const abs = join(repoPath, relPath);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, content);
  }
}

/**
 * Write a binary file (raw bytes) into a repo (uncommitted). Useful
 * for exercising the diff viewer's "Binary file changed" branch.
 */
export function writeBinaryFile(repoPath: string, relPath: string, bytes: Uint8Array): void {
  const abs = join(repoPath, relPath);
  mkdirSync(dirname(abs), { recursive: true });
  writeFileSync(abs, bytes);
}

/** Stage every change in the working tree and commit with `message`. */
export function commitAll(repoPath: string, message: string): void {
  runGit(repoPath, ["add", "-A"]);
  runGit(repoPath, ["commit", "-q", "-m", message]);
}

/**
 * Deterministic large-file content generator. Produces `lineCount`
 * lines, each unique so virtualization tests can grep for specific
 * mid-file lines without ambiguity.
 */
export function generateLargeFileContent(lineCount: number, prefix = "line"): string {
  const lines = new Array<string>(lineCount);
  for (let i = 0; i < lineCount; i++) {
    lines[i] = `${prefix} ${i}: ${"lorem ".repeat(8).trim()}`;
  }
  return lines.join("\n") + "\n";
}

/**
 * Minimal valid PNG byte sequence (8-byte signature + IHDR + IEND).
 * Just enough to make `git diff` classify the file as binary; not a
 * decodable image. Returned as a fresh `Uint8Array` so callers can
 * pass it to `writeBinaryFile` without sharing buffer state.
 */
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
