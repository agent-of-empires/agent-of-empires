// Live Playwright server: each handle owns a private HOME, tmux socket, and child.
// Readiness requires the child's post-bind URL announcement and an HTTP response.
// stop() waits for daemon, runner, and terminal groups before deleting the fixture.
// Failed teardown retains HOME and registry evidence rather than reporting success.

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, writeFileSync, chmodSync, mkdirSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { randomBytes } from "node:crypto";
import { expect } from "@playwright/test";
import { setTimeout as delay } from "node:timers/promises";
import { once } from "node:events";
import { isolateEnv } from "./isolatedEnv";
import { initWorkingRepo } from "./gitFixture";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const DEFAULT_PASSPHRASE = "aoe-e2e-fixed-passphrase";

export type AuthMode = "none" | "passphrase" | "token";

export interface SpawnOptions {
  authMode?: AuthMode;
  readOnly?: boolean;
  passphrase?: string;
  workerIndex: number;
  parallelIndex: number;
  /** Extra args to pass after the base `aoe serve` flags. */
  extraArgs?: string[];
  /** Override the spawn timeout (default 10s). */
  spawnTimeoutMs?: number;
  /**
   * When true and `authMode === "passphrase"`, the harness POSTs
   * `/api/login` itself after boot to mint a session cookie + record
   * the device binding secret. Useful for fixtures that need a
   * pre-authed browser context (e.g. a future acp-under-passphrase
   * spec). Defaults to false: specs that drive LoginPage end-to-end
   * (the `auth-login-passphrase` spec) want to start with no cookie
   * so the LoginPage actually renders.
   */
  preloginViaHarness?: boolean;
  /**
   * Token mode only. Sets `AOE_TEST_TOKEN_LIFETIME_SECS` on the server
   * subprocess; in debug builds the daemon enables the rotation task
   * even outside `--remote` and uses this lifetime. Ignored when
   * authMode !== "token".
   */
  tokenLifetimeSecs?: number;
  /**
   * Token mode only. Sets `AOE_TEST_TOKEN_GRACE_SECS`. Defaults to the
   * production 300s; specs that assert "old rejected past grace" pass a
   * small value (e.g. 2) so the assertion lands inside a Playwright run.
   */
  tokenGraceSecs?: number;
  /**
   * When true, install `fakeAcpAgent.mjs` as the `claude` / `aoe-agent`
   * shim instead of the tail-f-dev-null stub, and flip the structured view
   * master enable flag via `PATCH /api/acp/master` after the server
   * boots.
   */
  acp?: boolean;
  /** Optional path to a FAKE_ACP_SCRIPT for structured view tests. */
  fakeAcpScript?: string;
  /** Extra environment variables exported in the fake-ACP shim. Lets
   *  structured view tests toggle behavior on the fake agent (e.g. force a
   *  rejection of session/set_config_option) without writing a full
   *  scripted turn file. */
  extraEnv?: Record<string, string>;
  /**
   * Runs after the isolated $HOME tree is set up and the fake shim is on
   * PATH, but BEFORE `aoe serve` spawns. Use to call `aoe add` so the
   * server picks up the session record in-memory on boot (a post-spawn
   * `aoe add` would write to disk but the running server's
   * `state.instances` cache would never reload). The callback receives
   * the same env vars the server will run with, ready to pass straight
   * to `child_process.spawnSync(..., { env: seedEnv.env })`.
   */
  seedFn?: (seedEnv: {
    home: string;
    shimBin: string;
    xdg: string;
    tmp: string;
    tmuxTmp: string;
    env: NodeJS.ProcessEnv;
  }) => void | Promise<void>;
}

export interface ServeHandle {
  baseUrl: string;
  port: number;
  /** Root of the isolated filesystem tree (HOME / XDG / TMPDIR / TMUX_TMPDIR). */
  home: string;
  /** Directory prepended to PATH (contains the fake `claude` shim). */
  shimBin: string;
  /**
   * The exact env (isolated HOME / XDG bases / TMPDIR / PATH with the
   * shim) the daemon and seed ran with. Specs that drive `aoe` CLI
   * subprocesses against the same isolated state (e.g. `aoe session rename`
   * from a peer process) MUST pass this as `spawnSync(..., { env })`. Passing
   * `undefined` inherits the Playwright worker's env, which points at the
   * real `~/.config` and makes the CLI miss the seeded session.
   */
  env: NodeJS.ProcessEnv;
  proc: ChildProcess;
  authMode: AuthMode;
  passphrase?: string;
  /**
   * Token-mode only: the 64-char hex token the daemon wrote to
   * `serve.token` after boot. Specs append it as `?token=<value>` on
   * navigation or attach it as a Bearer header for direct fetches.
   */
  authToken?: string;
  /**
   * Token-mode only: filesystem path to `serve.token` under the
   * isolated HOME. Specs that need to read the rotated token re-read
   * this file; the daemon rewrites it on every rotation.
   */
  tokenFile?: string;
  /**
   * Set when `authMode === "passphrase"` and the harness has minted a
   * session via POST /api/login. Callers (typically the Playwright fixture)
   * inject this cookie into the browser context before navigation.
   */
  sessionCookie?: { name: string; value: string };
  /**
   * Stable base64url device binding secret the harness used at login time.
   * Specs that drive auth flows from the browser side need to seed the
   * same value into `localStorage` under `aoe-device-binding-secret`.
   */
  deviceBindingSecret?: string;
  /**
   * The tmux session prefix the running binary uses. Debug-mode builds
   * (`debug_assertions=true`, set by both `cargo build` and `cargo build
   * --profile dev-release`) use `aoe_dev_`; release builds use `aoe_`.
   * Specs that need to assert on tmux session names should compose this
   * with the session title rather than hard-coding `aoe_`.
   */
  tmuxPrefix: "aoe_" | "aoe_dev_";
  /**
   * The tmux socket the running binary uses (`AOE_TMUX_SOCKET`). Specs that
   * inspect sessions with a raw `tmux` call MUST pass `-S <this>`; debug
   * builds ignore `TMUX_TMPDIR` and route tmux through this socket (#2608).
   */
  tmuxSocket: string;
  stop(): Promise<void>;
  /**
   * Kill the running `aoe serve` proc and respawn it with the same args
   * on the same port. Used by connectivity-recovery specs (disconnect
   * banner) that need to observe the dashboard's `setServerDown(true)`
   * path on SIGTERM and then `setServerDown(false)` once the server is
   * back. The captured port is reused after the dead listener releases
   * it on `exit`. Token-mode reads the freshly written `serve.token`
   * and updates `handle.authToken`. Does NOT re-run passphrase
   * `preloginViaHarness` or structured view master enable; specs that need
   * those across a restart should call `spawnAoeServe` again.
   */
  restart(): Promise<void>;
}

/**
 * Fetch and unwrap `GET /api/sessions`. As of #1171 the response shape is
 * `{ sessions: SessionResponse[], workspace_ordering: string[] }`. Callers
 * typically want only the sessions array, so this helper hides the
 * envelope change so a future shape tweak is one edit away.
 */
export async function listSessions(
  baseUrl: string,
): Promise<Array<{ id: string; title: string; status: string; [k: string]: unknown }>> {
  const res = await fetch(`${baseUrl}/api/sessions`);
  if (!res.ok) {
    throw new Error(`GET /api/sessions failed: ${res.status} ${await res.text()}`);
  }
  const body = await res.json();
  if (Array.isArray(body)) return body;
  if (body && Array.isArray(body.sessions)) return body.sessions;
  throw new Error(`GET /api/sessions returned an unexpected shape: ${JSON.stringify(body).slice(0, 200)}`);
}

/**
 * Poll `GET /api/sessions` until at least one session is present, and
 * return the snapshot the poll settled on. The list is a cache the
 * daemon reconciles on a 2s tick (see `waitForView` below), so a fresh
 * `listSessions()` issued right after a poll that already saw the
 * session can come back empty; reading the array from inside the poll
 * removes that second, racy fetch.
 */
export async function waitForSessions(
  baseUrl: string,
  timeout = 15_000,
): Promise<Awaited<ReturnType<typeof listSessions>>> {
  let settled: Awaited<ReturnType<typeof listSessions>> = [];
  await expect
    .poll(
      async () => {
        settled = await listSessions(baseUrl);
        return settled.length;
      },
      {
        timeout,
        intervals: [100, 200, 400],
        message: `at least one session should appear in GET /api/sessions within ${timeout}ms`,
      },
    )
    .toBeGreaterThan(0);
  return settled;
}

/**
 * Poll `GET /api/sessions` until the given session's `view` reaches
 * `expected`. The sessions list is a cache the daemon reconciles on a
 * 2s tick, so a disk snapshot taken just before an endpoint's write
 * can briefly clobber the in-memory view before self-correcting; a
 * bare `expect(sessions.find(...).view === expected)` on the very
 * next `listSessions()` after a view-mutating endpoint is the flake
 * shape. The endpoint's own response body remains the authoritative
 * synchronous check; use this helper for reads that go back through
 * the sessions list.
 *
 * The server omits the `view` field for `Terminal` sessions (serde
 * `skip_serializing_if`); this helper treats a missing field on a
 * present session as `"terminal"` so callers pass one of two
 * symmetric string values. A missing session (unknown or deleted id)
 * is not coerced: the callback throws, and since `expect.poll`
 * propagates a thrown callback immediately (only a failed matcher is
 * retried), this fails fast on a bad or never-created id rather than
 * false-passing `.toBe("terminal")`.
 */
export async function waitForView(
  baseUrl: string,
  sessionId: string,
  expected: "structured" | "terminal",
  timeout = 10_000,
): Promise<void> {
  await expect
    .poll(
      async () => {
        const sessions = await listSessions(baseUrl);
        const session = sessions.find((s) => s.id === sessionId);
        if (session === undefined) {
          throw new Error(`session ${sessionId} not found in listSessions`);
        }
        return session.view ?? "terminal";
      },
      {
        timeout,
        intervals: [100, 200, 400],
        message: `session ${sessionId} view should converge to ${expected}`,
      },
    )
    .toBe(expected);
}

/**
 * Returns a `seedFn` for `spawnAoeServe` that:
 *   1. git-inits a fresh project dir under the isolated HOME.
 *   2. runs `aoe add <projectDir> -t <title> -c <tool>` against the same env.
 *
 * Must run BEFORE serve spawns so the server picks up the session record
 * in-memory on boot. A post-spawn `aoe add` writes to disk but the running
 * server's `state.instances` cache never reloads, so subsequent
 * `GET /api/sessions` returns an empty list.
 */
export function seedSessionViaAoeAdd(opts: {
  title: string;
  tool?: string;
  subdir?: string;
}): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  return ({ home, env }) => {
    const projectDir = join(home, opts.subdir ?? "project");
    initWorkingRepo(projectDir, env);
    const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", opts.title, "-c", opts.tool ?? "claude"], {
      env,
    });
    if (addRes.status !== 0) {
      throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
    }
  };
}

export function resolveAoeBinary(): string {
  const fromEnv = process.env.AOE_E2E_BINARY;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  const repoRoot = resolve(__dirname, "..", "..", "..");
  // Live tests require debug-only timing overrides. CI also supplies a debug binary.
  const debug = join(repoRoot, "target", "debug", "aoe");
  if (existsSync(debug)) return debug;
  return join(repoRoot, "target", "release", "aoe");
}

/**
 * Map a resolved aoe binary path to the tmux session prefix the binary
 * will use. The Rust side sets the prefix at compile time based on
 * `cfg!(debug_assertions)`; we can't query it from JS, so we derive it
 * from the build directory in the path. CI passes the binary via
 * `AOE_E2E_BINARY` so this works in CI; locally it falls through to the
 * debug/release fallback in `resolveAoeBinary`.
 */
export function tmuxPrefixFor(binaryPath: string): "aoe_" | "aoe_dev_" {
  return binaryPath.includes("/target/debug/") ? "aoe_dev_" : "aoe_";
}

/**
 * The tmux socket path the harness pins via `AOE_TMUX_SOCKET`, under the
 * test's isolated tmux tmpdir. The daemon and every spec that shells out to a
 * raw `tmux` must agree on this: debug builds ignore `TMUX_TMPDIR` and route
 * tmux through an explicit `-S <socket>` (#2608).
 */
export function tmuxSocketPath(home: string): string {
  return join(home, "tmux", "aoe.sock");
}

/**
 * Resolve where the daemon will write `serve.token` (and other serve.*
 * state files) under the test's isolated filesystem tree. Mirrors the
 * Rust `get_app_dir_path` logic at `src/session/mod.rs:83`: Linux uses
 * `$XDG_CONFIG_HOME/agent-of-empires[-dev]`. Debug builds carry the `-dev`
 * suffix, derived from the binary path the same way as `tmuxPrefixFor`.
 *
 * macOS/Windows go through `session::macos_app_dir` (#1948), whose precedence
 * is: the XDG path if it exists, else the legacy `~/.agent-of-empires` if that
 * exists, else the XDG path whenever `XDG_CONFIG_HOME` is set, else legacy.
 * The harness always sets `XDG_CONFIG_HOME` on an isolated tree, so a fresh
 * test home resolves to the XDG path, NOT the legacy one. Returning the legacy
 * path unconditionally here pointed specs at a directory the daemon never
 * touches, which reads as a passing no-op on macOS while CI (Linux) took the
 * correct branch.
 */
export function appDirFor(home: string, xdg: string, binaryPath: string): string {
  const suffix = binaryPath.includes("/target/debug/") ? "-dev" : "";
  const xdgDir = join(xdg, `agent-of-empires${suffix}`);
  if (process.platform === "linux") {
    return xdgDir;
  }
  const legacy = join(home, `.agent-of-empires${suffix}`);
  if (existsSync(xdgDir)) return xdgDir;
  if (existsSync(legacy)) return legacy;
  return xdg ? xdgDir : legacy;
}

interface ProcessSnapshot {
  pid: number;
  parent: number;
  group: number;
  command: string;
}

function processSnapshot(env: NodeJS.ProcessEnv): ProcessSnapshot[] {
  const result = spawnSync("ps", ["-ww", "-axo", "pid=,ppid=,pgid=,stat=,args="], {
    env: { ...env, LC_ALL: "C" },
    encoding: "utf8",
    timeout: 2000,
  });
  if (result.status !== 0) throw new Error(`cannot inspect fixture processes: ${result.error ?? result.stderr}`);
  return result.stdout.split("\n").flatMap((line) => {
    const match = line.match(/^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/);
    if (!line.trim()) return [];
    if (!match) throw new Error(`unrecognized ps output: ${line}`);
    if (match[4].startsWith("Z")) return [];
    return [{ pid: Number(match[1]), parent: Number(match[2]), group: Number(match[3]), command: match[5] }];
  });
}

/** Revoke the private lease; the runner watchdog terminates its own process group. */
async function stopOrphanRunners(appDir: string, binary: string, env: NodeJS.ProcessEnv): Promise<void> {
  const { readdirSync, readFileSync, renameSync } = await import("node:fs");
  const workersDir = join(appDir, "acp-workers");
  if (!existsSync(workersDir)) return;
  const executable = realpathSync(binary);
  const socketPrefix = `${executable} __acp-runner --socket ${workersDir}/`;
  // A runner can unlink its record before exiting. Keep its observed group even
  // when enumeration, reading, or lease revocation races that normal transition.
  const groups = new Set(
    processSnapshot(env)
      .filter((p) => p.pid === p.group && p.command.startsWith(socketPrefix))
      .map((p) => p.group),
  );
  const records = readdirSync(workersDir).filter((name) => name.endsWith(".json") || name.endsWith(".json.stopping"));
  for (const name of records) {
    const path = join(workersDir, name);
    let raw: string;
    try {
      raw = readFileSync(path, "utf8");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
      throw error;
    }
    const { pid, session_id: sessionId, socket_path: socketPath } = JSON.parse(raw);
    // JSON.parse rounds u64 epochs; preserve the decimal argument exactly.
    const generation = raw.match(/"generation"\s*:\s*(\d+)/)?.[1];
    const recordName = name.replace(/\.stopping$/, "");
    if (
      !Number.isSafeInteger(pid) ||
      pid <= 1 ||
      typeof sessionId !== "string" ||
      !generation ||
      recordName !== `${sessionId}.json` ||
      socketPath !== join(workersDir, `${sessionId}.sock`)
    ) {
      throw new Error(`invalid runner identity in ${name}; retaining ${appDir}`);
    }
    const processes = processSnapshot(env);
    const runner = processes.find((p) => p.pid === pid);
    const members = processes.filter((p) => p.group === pid);
    if (!runner && members.length === 0) continue;
    const prefix = `${executable} __acp-runner --socket ${socketPath} --session-id ${sessionId} `;
    if (
      !runner ||
      runner.group !== pid ||
      !runner.command.startsWith(prefix) ||
      !runner.command.split(" -- ")[0].endsWith(` --generation ${generation}`)
    ) {
      throw new Error(`runner ${pid} no longer matches ${name}; retaining ${appDir}`);
    }
    // Preserve recovery evidence until exit is observed. Never signal this numeric PID.
    if (name === recordName) {
      try {
        renameSync(path, `${path}.stopping`);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
    }
    groups.add(pid);
  }
  for (const process of processSnapshot(env)) {
    if (process.pid === process.group && process.command.startsWith(socketPrefix)) groups.add(process.group);
  }
  if (groups.size === 0) return;
  // Older binaries use two 10s watchdog polls, then a bounded 2s agent shutdown.
  const deadline = performance.now() + 25_000;
  while (processSnapshot(env).some((p) => groups.has(p.group))) {
    if (performance.now() >= deadline) throw new Error(`runner groups did not exit; retaining ${appDir}`);
    await delay(50);
  }
}

async function stopTerminalProcesses(
  socket: string,
  shimBin: string | undefined,
  env: NodeJS.ProcessEnv,
): Promise<void> {
  if (!existsSync(socket) && shimBin === undefined) return;
  const options = { env: { ...env, LC_ALL: "C" }, encoding: "utf8" as const, timeout: 2000 };
  const owned = new Set<number>();
  if (existsSync(socket)) {
    const panes = spawnSync("tmux", ["-S", socket, "list-panes", "-a", "-F", "#{pid} #{pane_pid}"], options);
    if (panes.status === 0) {
      for (const value of panes.stdout.trim() ? panes.stdout.trim().split(/\s+/) : []) {
        const pid = Number(value);
        if (!Number.isSafeInteger(pid) || pid <= 1) throw new Error(`invalid private tmux process identity: ${value}`);
        owned.add(pid);
      }
    } else if (panes.error || (existsSync(socket) && panes.stderr.trim() !== `no server running on ${socket}`)) {
      throw new Error(`cannot inspect private tmux server: ${panes.error ?? panes.stderr}`);
    }
  }
  const processes = processSnapshot(env);
  if (shimBin !== undefined) {
    for (const entry of processes) {
      if (entry.command.startsWith(`${shimBin}/`)) owned.add(entry.pid);
    }
  }
  let expanded = true;
  while (expanded) {
    expanded = false;
    for (const entry of processes) {
      if (owned.has(entry.parent) && !owned.has(entry.pid)) {
        owned.add(entry.pid);
        expanded = true;
      }
    }
  }
  const groups = new Set(processes.filter((entry) => owned.has(entry.pid)).map((entry) => entry.group));
  if (existsSync(socket)) {
    const killed = spawnSync("tmux", ["-S", socket, "kill-server"], options);
    if (
      killed.error ||
      (killed.status !== 0 && existsSync(socket) && killed.stderr.trim() !== `no server running on ${socket}`)
    ) {
      throw new Error(`cannot stop private tmux server: ${killed.error ?? killed.stderr}`);
    }
  }
  // kill-server acknowledges the command before terminal descendants finish exiting.
  const deadline = performance.now() + 4000;
  while (processSnapshot(env).some((entry) => groups.has(entry.group))) {
    if (performance.now() >= deadline) throw new Error(`terminal groups did not exit; retaining ${socket}`);
    await delay(50);
  }
}

/**
 * Wait for `serve.token` to appear in the daemon's app dir, then read
 * it. The daemon writes the token early in startup, so by the time
 * `waitForServer` resolves it is on disk; the loop is a small safety
 * net for systems where fs writes lag the listen socket by a few ms.
 */
async function readTokenFile(tokenPath: string, deadlineMs: number): Promise<string> {
  const { readFile } = await import("node:fs/promises");
  const deadline = Date.now() + deadlineMs;
  let lastErr: unknown = "no attempts made";
  while (Date.now() < deadline) {
    try {
      const raw = await readFile(tokenPath, "utf8");
      const token = raw.trim();
      if (token.length > 0) return token;
      lastErr = "empty";
    } catch (err) {
      lastErr = err;
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`token file ${tokenPath} not readable: ${lastErr}`);
}

function portFor(workerIndex: number, parallelIndex: number, attempt: number): number {
  // 5200 + worker*100 + parallel + attempt*7 covers ~14 retries per
  // (worker, parallel) slot before colliding with the next slot.
  return 5200 + workerIndex * 100 + parallelIndex + attempt * 7;
}

async function waitForServer(
  baseUrl: string,
  deadlineMs: number,
  proc: ChildProcess,
  authMode: AuthMode,
  bound: () => boolean,
  spawnError: () => Error | undefined,
): Promise<void> {
  const deadline = performance.now() + deadlineMs;
  let lastErr: unknown = "child has not announced its bound URL";
  while (performance.now() < deadline) {
    if (spawnError()) throw spawnError();
    if (proc.exitCode !== null || proc.signalCode !== null) {
      throw new Error(`aoe serve died before ready (exit=${proc.exitCode} signal=${proc.signalCode})`);
    }
    if (bound()) {
      try {
        const res = await fetch(`${baseUrl}/api/about`, {
          signal: AbortSignal.timeout(Math.max(1, Math.ceil(deadline - performance.now()))),
        });
        await res.body?.cancel();
        if (proc.exitCode !== null || proc.signalCode !== null) continue;
        if (res.status === 200 || (authMode !== "none" && res.status === 401)) return;
        lastErr = `status ${res.status}`;
      } catch (err) {
        lastErr = err;
      }
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`aoe serve at ${baseUrl} not ready: ${lastErr}`);
}

function writeFakeClaudeShim(binDir: string): void {
  // Dashboard tracer specs only need the tmux pane to stay open with a
  // long-running process. Structured view specs swap this for the ACP agent shim
  // via `writeFakeAcpShim`. Install shims for the built-in agents the
  // wizard UI surfaces (claude / codex / gemini); the agent picker
  // filters by `which <binary>` (src/tmux/mod.rs::is_agent_available),
  // so without these the picker only offers claude and persistence
  // specs that pick a non-default tool would hang on a missing button.
  const script = "#!/bin/bash\nexec tail -f /dev/null\n";
  for (const name of ["claude", "codex", "gemini", "opencode"]) {
    const path = join(binDir, name);
    writeFileSync(path, script);
    chmodSync(path, 0o755);
  }
}

function writeFakeAcpShim(
  binDir: string,
  fakeAcpScript: string | undefined,
  fakeAcpDebugLog: string,
  extraEnv: Record<string, string> | undefined,
): void {
  // The structured view supervisor resolves the agent through `AgentRegistry`
  // (src/acp/agent_registry.rs): the `claude` tool key maps to
  // command `claude-agent-acp`, not `claude`. `resolve_agent_command`
  // walks $PATH and node-version dirs, so without a `claude-agent-acp`
  // entry in the shim dir the supervisor falls through to the real
  // installed adapter, which then surfaces "Authentication required"
  // on the first prompt. Shim every name a structured view test can land on.
  //
  // The shim also re-exports diagnostic env vars (FAKE_ACP_SCRIPT,
  // FAKE_ACP_DEBUG_LOG) so they reach the node child even when the
  // daemon -> runner spawn chain does not propagate every env from
  // the parent (observed in CI: seedEnv vars set on `aoe serve` do
  // not all reach the runner-spawned fake-ACP child, so relying on
  // process.env in fakeAcpAgent.mjs alone is unreliable).
  const fakeAgentJs = resolve(__dirname, "fakeAcpAgent.mjs");
  const scriptLines: string[] = [];
  if (fakeAcpScript) {
    scriptLines.push(`export FAKE_ACP_SCRIPT=${JSON.stringify(fakeAcpScript)}`);
  } else {
    scriptLines.push("unset FAKE_ACP_SCRIPT");
  }
  scriptLines.push(`export FAKE_ACP_DEBUG_LOG=${JSON.stringify(fakeAcpDebugLog)}`);
  for (const [key, value] of Object.entries(extraEnv ?? {})) {
    scriptLines.push(`export ${key}=${JSON.stringify(value)}`);
  }
  for (const name of ["claude", "claude-agent-acp", "aoe-agent", "opencode", "codex", "codex-acp"]) {
    // The agent_compat gate keys its version floor off the spawned binary
    // name. When the fake stands in for opencode it must report opencode's
    // handshake (name + a version at or above the opencode floor), or the
    // gate rejects it and the opencode live specs fail; FAKE_ACP_IMPERSONATE
    // tells fakeAcpAgent.mjs which identity to present.
    //
    // `codex` (the native CLI) is shimmed alongside `codex-acp` (the ACP
    // adapter the supervisor actually spawns) because the wizard's agent
    // picker only renders agents whose native binary is detected on PATH
    // (`AvailableTools::detect` -> `DetectionMethod::Which("codex")`). Without
    // a `codex` shim the picker button never appears and codex-selecting specs
    // time out, even though the ACP spawn resolves `codex-acp`.
    const perName =
      name === "opencode"
        ? [...scriptLines, "export FAKE_ACP_IMPERSONATE=opencode"]
        : name === "codex-acp" || name === "codex"
          ? [...scriptLines, "export FAKE_ACP_IMPERSONATE=codex"]
          : scriptLines;
    // Keep orphaned agents attributable after their tmux pane disappears.
    const path = join(binDir, name);
    const script = `#!/bin/bash\n${perName.join("\n")}\nexec -a ${JSON.stringify(path)} ${JSON.stringify(process.execPath)} ${JSON.stringify(fakeAgentJs)} "$@"\n`;
    writeFileSync(path, script);
    chmodSync(path, 0o755);
  }
}

async function loginWithPassphrase(
  baseUrl: string,
  passphrase: string,
  deviceBindingSecret: string,
): Promise<{ cookie: { name: string; value: string } }> {
  const res = await fetch(`${baseUrl}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      passphrase,
      device_binding_secret: deviceBindingSecret,
    }),
  });
  if (!res.ok) {
    throw new Error(`POST /api/login failed: ${res.status} ${await res.text()}`);
  }
  const setCookie = res.headers.get("set-cookie") ?? "";
  // axum returns a single Set-Cookie; cookie name we want is "aoe_session".
  const match = /aoe_session=([^;]+)/.exec(setCookie);
  if (!match) {
    throw new Error(`POST /api/login did not set aoe_session cookie. Set-Cookie was: ${setCookie}`);
  }
  return { cookie: { name: "aoe_session", value: match[1] } };
}

export async function spawnAoeServe(opts: SpawnOptions): Promise<ServeHandle> {
  const aoeBinary = resolveAoeBinary();
  if (!existsSync(aoeBinary)) {
    throw new Error(
      `aoe binary not found at ${aoeBinary}. ` + `Set AOE_E2E_BINARY or run liveGlobalSetup.ts to build it.`,
    );
  }

  // realpathSync resolves any symlinks in the tmpdir path (on macOS,
  // `/var/folders/...` lives under `/private/var/...`). The server's
  // `/api/filesystem/browse` endpoint canonicalizes the requested path
  // and checks `starts_with(dirs::home_dir())`; if HOME is the un-
  // canonicalized form, that check fails on macOS and any browse call
  // against the test's HOME tree returns "outside the home directory".
  //
  // Use `/tmp/...` as the base instead of `tmpdir()`. On macOS,
  // `tmpdir()` resolves to `/private/var/folders/<hash>/T/...` (~95
  // chars). After we append `/.agent-of-empires-dev/acp-workers/
  // <session_id>.sock` (~60 chars) we blow past the 104-byte
  // `sun_path` limit on Darwin unix sockets and the runner's
  // `UnixListener::bind` fails with ENAMETOOLONG. Because the runner
  // writes its stderr to /dev/null, the failure surfaces as "runner
  // socket … did not appear within Ns" (the daemon's wait_for_socket
  // poll never sees a socket appear) instead of a typed bind error.
  // `/tmp` is a stable, short, world-writable directory on every
  // supported OS we target; using it caps the path well under
  // sun_path on Darwin (104) and Linux (108). See macOS sun_path
  // <sys/un.h>.
  // Windows has no `/tmp`; fall back to `tmpdir()` there. `sun_path`
  // is a POSIX-only limit, so the Darwin short-path workaround does
  // not apply to win32 either.
  const shortBase = process.platform === "win32" ? tmpdir() : "/tmp";
  const home = realpathSync(mkdtempSync(join(shortBase, `aoe-pw-w${opts.workerIndex}-p${opts.parallelIndex}-`)));
  const xdg = join(home, "config");
  const xdgData = join(home, "share");
  const tmp = join(home, "tmp");
  const tmuxTmp = join(home, "tmux");
  const shimBin = join(home, "bin");
  for (const dir of [xdg, xdgData, tmp, tmuxTmp, shimBin]) {
    mkdirSync(dir, { recursive: true, mode: 0o700 });
  }
  const appDir = appDirFor(home, xdg, aoeBinary);
  mkdirSync(appDir, { recursive: true, mode: 0o700 });
  // General live tests exercise launches, not the one-time TUI approval flow.
  writeFileSync(join(appDir, "config.toml"), "[app_state]\nhas_acknowledged_agent_hooks = true\n");
  const fakeAcpDebugLog = join(home, "fake-acp.log");
  if (opts.acp) {
    writeFakeAcpShim(shimBin, opts.fakeAcpScript, fakeAcpDebugLog, opts.extraEnv);
  } else {
    writeFakeClaudeShim(shimBin);
  }

  const authMode: AuthMode = opts.authMode ?? "none";

  const seedEnv: NodeJS.ProcessEnv = {
    // The isolated HOME is only isolated if nothing overrides where the agents
    // read their config and data from. See `isolatedEnv.ts`.
    ...isolateEnv(process.env, { home, xdgConfig: xdg, xdgData, tmp, tmuxTmp }),
    PATH: `${shimBin}:${process.env.PATH ?? ""}`,
    // Lift the runner-socket appearance deadline. The `aoe
    // __acp-runner` shim re-execs the debug `aoe` binary, which
    // under v8 coverage + 3 parallel workers + tmux + a fake-ACP node
    // subprocess can take >10s to bind its unix listener on a
    // contended runner. The production 10s default in
    // `runner_socket_deadline()` covers cold caches; tests need
    // headroom or `acp_enable` fails with `runner socket … did
    // not appear within 10s` (deterministic on slower local + CI
    // machines, never on hot caches). Honored only in debug builds.
    AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS: "60000",
    // Teardown revokes the registry lease and waits for this runner-owned watchdog.
    AOE_ACP_WATCHDOG_POLL_MS: "100",
    // FAKE_ACP_DEBUG_LOG is *also* re-exported by the shim itself
    // (see writeFakeAcpShim) because the daemon -> runner -> node
    // spawn chain on CI Linux did not propagate this env var from
    // process.env alone. Keeping it on seedEnv too is harmless and
    // covers any future caller that bypasses the shim path.
    FAKE_ACP_DEBUG_LOG: fakeAcpDebugLog,
    // Daemon log level. AOE_LOG_LEVEL only accepts a single level
    // string (trace|debug|info|warn|error); see LogLevel::parse in
    // src/logging.rs. The default `info` is sufficient for the
    // post-mortem attachments; `trace` was used briefly to diagnose
    // the XDG_CONFIG_HOME bug but adds enough I/O pressure on CI to
    // cause unrelated REST flakes (e.g. settings PATCH failing
    // under contention and triggering an optimistic-update revert).
    // Override via process env if a future investigation needs it.
    AOE_LOG_LEVEL: process.env.AOE_LOG_LEVEL ?? "info",
    // Suppress the first-load telemetry consent modal. Every live spec boots
    // a fresh HOME where `has_responded_to_telemetry` is false, so the modal
    // (`telemetry-modal-title`, a z-50 full-screen backdrop) would otherwise
    // intercept pointer events and time out every `click`. `DO_NOT_TRACK`
    // makes `/api/telemetry/status` report `do_not_track: true`, which App.tsx
    // treats as "never auto-show the modal". The consent flow itself is
    // covered by the Vitest + RTL contract tests, not the live suite. A future
    // live spec that exercises the modal can unset this in its own env.
    DO_NOT_TRACK: process.env.DO_NOT_TRACK ?? "1",
    // Pin the tmux socket explicitly. Debug builds otherwise route tmux
    // through `<app_dir>/tmux.sock` and ignore TMUX_TMPDIR (#2608), so specs
    // that inspect sessions with a raw `tmux` call must target this same
    // socket (see `tmuxSocketPath`) rather than the default one under
    // TMUX_TMPDIR.
    AOE_TMUX_SOCKET: tmuxSocketPath(home),
  };

  if (authMode === "token") {
    if (typeof opts.tokenLifetimeSecs === "number") {
      seedEnv.AOE_TEST_TOKEN_LIFETIME_SECS = String(opts.tokenLifetimeSecs);
    }
    if (typeof opts.tokenGraceSecs === "number") {
      seedEnv.AOE_TEST_TOKEN_GRACE_SECS = String(opts.tokenGraceSecs);
    }
  }

  const passphrase = authMode === "passphrase" ? (opts.passphrase ?? DEFAULT_PASSPHRASE) : undefined;

  const spawnTimeoutMs = opts.spawnTimeoutMs ?? 10_000;

  function buildArgs(boundPort: number): string[] {
    const args = ["serve", "--host", "127.0.0.1", "--port", String(boundPort)];
    if (authMode === "none") args.push("--no-auth");
    if (authMode === "token") args.push("--auth", "token");
    if (authMode === "passphrase") {
      // `--passphrase X` alone leaves the auth mode at the default
      // (Token + passphrase as 2FA). The Playwright browser has no
      // token, so `/api/login/status` 401s on the no-token branch in
      // `auth_middleware` before any login-exempt or loopback-bypass
      // check, and the SPA renders TokenEntryPage instead of LoginPage.
      // `--auth=passphrase` switches the server into the
      // `run_passphrase_wall` path where `/api/login` and
      // `/api/login/status` are login-exempt, so the SPA can bootstrap
      // and LoginPage actually renders. See #1230.
      args.push("--auth", "passphrase");
    }
    if (passphrase) args.push("--passphrase", passphrase);
    if (opts.readOnly) args.push("--read-only");
    if (opts.extraArgs) args.push(...opts.extraArgs);
    return args;
  }

  async function spawnOnce(args: string[], boundBaseUrl: string): Promise<ChildProcess> {
    const child = spawn(aoeBinary, args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: seedEnv,
    });
    let spawnError: Error | undefined;
    child.once("error", (error) => {
      spawnError = error;
    });
    let bound = false;
    let line = "";
    // startup.rs emits this URL on this child's pipe only after TcpListener::bind.
    child.stdout?.on("data", (chunk) => {
      line += chunk.toString();
      let newline: number;
      while ((newline = line.indexOf("\n")) !== -1) {
        const message = line.slice(0, newline).trim();
        line = line.slice(newline + 1);
        if (message === `${boundBaseUrl}/` || message.startsWith(`${boundBaseUrl}/?token=`)) bound = true;
      }
      line = line.slice(-8192);
    });
    child.stderr?.resume();
    if (process.env.AOE_E2E_DEBUG === "1") {
      const { createWriteStream } = await import("node:fs");
      const log = createWriteStream(join(home, "serve.log"), { flags: "a" });
      child.stdout?.on("data", (b) => log.write(b));
      child.stderr?.on("data", (b) => log.write(b));
      child.once("close", () => log.end());
    }
    pendingChildren.add(child);
    try {
      await waitForServer(
        boundBaseUrl,
        spawnTimeoutMs,
        child,
        authMode,
        () => bound,
        () => spawnError,
      );
      return child;
    } catch (error) {
      await killProc(child);
      throw error;
    }
  }

  const pendingChildren = new Set<ChildProcess>();
  let proc: ChildProcess | null = null;
  let port = 0;
  let baseUrl = "";

  async function killProc(child: ChildProcess): Promise<void> {
    if (child.exitCode !== null || child.signalCode !== null || child.pid === undefined) {
      pendingChildren.delete(child);
      return;
    }
    const exited = once(child, "exit", { signal: AbortSignal.timeout(4000) });
    const escalate = setTimeout(() => child.kill("SIGKILL"), 2000);
    try {
      child.kill("SIGTERM");
      await exited;
      pendingChildren.delete(child);
    } catch (error) {
      throw new Error(`aoe child ${child.pid} did not exit; retaining ${home}`, { cause: error });
    } finally {
      clearTimeout(escalate);
    }
  }

  async function cleanup(): Promise<void> {
    const errors: unknown[] = [];
    // Stop the daemon before its runners, so reconciliation cannot respawn them.
    for (const child of pendingChildren) {
      try {
        await killProc(child);
      } catch (error) {
        errors.push(error);
      }
    }
    if (errors.length === 0) {
      try {
        await stopOrphanRunners(appDir, aoeBinary, seedEnv);
      } catch (error) {
        errors.push(error);
      }
    }
    try {
      await stopTerminalProcesses(tmuxSocketPath(home), opts.acp ? shimBin : undefined, seedEnv);
    } catch (error) {
      errors.push(error);
    }
    if (errors.length) throw new AggregateError(errors, `teardown incomplete; retaining ${home}`);
    rmSync(home, { recursive: true, force: true });
  }

  try {
    if (opts.seedFn) await opts.seedFn({ home, shimBin, xdg, tmp, tmuxTmp, env: seedEnv });
    for (let attempt = 0; attempt < 5; attempt++) {
      port = portFor(opts.workerIndex, opts.parallelIndex, attempt);
      baseUrl = `http://127.0.0.1:${port}`;
      try {
        proc = await spawnOnce(buildArgs(port), baseUrl);
        break;
      } catch (error) {
        if (attempt === 4 || pendingChildren.size > 0) throw error;
      }
    }
    if (!proc) throw new Error("aoe serve failed to bind on every attempted port");
    let authToken: string | undefined;
    let tokenFile: string | undefined;
    if (authMode === "token") {
      tokenFile = join(appDirFor(home, xdg, aoeBinary), "serve.token");
      authToken = await readTokenFile(tokenFile, spawnTimeoutMs);
    }
    const handle: ServeHandle = {
      baseUrl,
      port,
      home,
      shimBin,
      env: seedEnv,
      proc,
      authMode,
      passphrase,
      authToken,
      tokenFile,
      tmuxPrefix: tmuxPrefixFor(aoeBinary),
      tmuxSocket: tmuxSocketPath(home),
      async restart() {
        if (proc) await killProc(proc);
        const next = await spawnOnce(buildArgs(port), baseUrl);
        proc = next;
        handle.proc = next;
        if (authMode === "token" && tokenFile) {
          const refreshed = await readTokenFile(tokenFile, spawnTimeoutMs);
          handle.authToken = refreshed;
        }
      },
      stop: cleanup,
    };

    if (authMode === "passphrase" && passphrase && opts.preloginViaHarness) {
      const deviceBindingSecret = randomBytes(32).toString("base64url");
      const { cookie } = await loginWithPassphrase(baseUrl, passphrase, deviceBindingSecret);
      handle.sessionCookie = cookie;
      handle.deviceBindingSecret = deviceBindingSecret;
    }

    return handle;
  } catch (error) {
    try {
      await cleanup();
    } catch (teardownError) {
      throw new AggregateError([error, teardownError], `startup failed and teardown incomplete; retaining ${home}`, {
        cause: teardownError,
      });
    }
    throw error;
  }
}
