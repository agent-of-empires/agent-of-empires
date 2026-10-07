// Live Playwright server: each handle owns a private HOME, tmux socket, and child.
// Readiness requires the child's post-bind URL announcement and an HTTP response.
// stop() waits for daemon, runner, and terminal groups before deleting the fixture.
// Failed teardown retains HOME and registry evidence rather than reporting success.

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  writeFileSync,
  chmodSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { randomBytes } from "node:crypto";
import { expect } from "@playwright/test";
import { setTimeout as delay } from "node:timers/promises";
import { once } from "node:events";
import { isolateEnv } from "./isolatedEnv";
import { commitAll, initWorkingRepo, writeFiles } from "./gitFixture";

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
  /** Passphrase mode: log in after boot so `seedAuth` can pre-authenticate the browser. */
  preloginViaHarness?: boolean;
  /** Token mode: token lifetime (debug builds rotate even without `--remote`). */
  tokenLifetimeSecs?: number;
  /** Token mode: grace period for the previous token (production default 300s). */
  tokenGraceSecs?: number;
  /** Install `fakeAcpAgent.mjs` as every agent shim instead of an idle stub. */
  acp?: boolean;
  /** FAKE_ACP_SCRIPT path, or a script object written into the isolated HOME. */
  fakeAcpScript?: string | object;
  /** Extra env exported in the fake-ACP shim; a function receives the isolated HOME. */
  extraEnv?: Record<string, string> | ((home: string) => Record<string, string>);
  /** Seeds state before `aoe serve` boots; the server never reloads sessions added later. */
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
  home: string;
  shimBin: string;
  /** The isolated env; pass it to any `aoe` CLI subprocess or it reads the real config. */
  env: NodeJS.ProcessEnv;
  proc: ChildProcess;
  authMode: AuthMode;
  passphrase?: string;
  authToken?: string;
  /** Token mode: rewritten on every rotation. */
  tokenFile?: string;
  sessionCookie?: { name: string; value: string };
  deviceBindingSecret?: string;
  /** `aoe_dev_` for debug builds, `aoe_` for release. */
  tmuxPrefix: "aoe_" | "aoe_dev_";
  /** Raw `tmux` calls must pass `-S <tmuxSocket>`. */
  tmuxSocket: string;
  stop(): Promise<void>;
  /** Kill and respawn on the same port; does not repeat harness login or structured view enable. */
  restart(): Promise<void>;
}

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

/** Return the first non-empty sessions snapshot; a second fetch can race the daemon's reconcile tick. */
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
 * Poll a session's view through the reconciled sessions cache. A missing `view` means terminal;
 * an unknown session throws, which fails the poll immediately.
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
 * A `seedFn` that creates `~/<subdir>` (a git repo unless `git: false`), commits `committed`, writes
 * `files` uncommitted, runs `prepare`, and registers the directory with `aoe add`.
 */
export function seedSessionViaAoeAdd(opts: {
  title: string;
  tool?: string;
  subdir?: string;
  git?: boolean;
  committed?: Record<string, string>;
  files?: Record<string, string>;
  prepare?: (projectDir: string, env: NodeJS.ProcessEnv) => void;
  /** Bash script run as the agent via `--cmd-override`, by absolute path so no real agent is resolved. */
  agentScript?: string;
}): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  return ({ home, shimBin, env }) => {
    const projectDir = join(home, opts.subdir ?? "project");
    if (opts.git === false) mkdirSync(projectDir, { recursive: true });
    else initWorkingRepo(projectDir, env);
    if (opts.committed) {
      writeFiles(projectDir, opts.committed);
      commitAll(projectDir, "baseline", env);
    }
    if (opts.files) writeFiles(projectDir, opts.files);
    opts.prepare?.(projectDir, env);
    const args = ["add", projectDir, "-t", opts.title, "-c", opts.tool ?? "claude"];
    if (opts.agentScript) {
      const script = join(shimBin, `${opts.title}-agent`);
      writeFileSync(script, opts.agentScript, { mode: 0o755 });
      args.push("--cmd-override", script);
    }
    const addRes = spawnSync(resolveAoeBinary(), args, { env });
    if (addRes.status !== 0) {
      throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
    }
  };
}

export function fakeAcpScriptPath(home: string): string {
  return join(home, "fake-acp-script.json");
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

export function tmuxPrefixFor(binaryPath: string): "aoe_" | "aoe_dev_" {
  return binaryPath.includes("/target/debug/") ? "aoe_dev_" : "aoe_";
}

export function tmuxSocketPath(home: string): string {
  return join(home, "tmux", "aoe.sock");
}

/**
 * Mirrors the daemon's app dir: XDG on Linux; elsewhere XDG if present, then legacy if present,
 * then XDG whenever XDG_CONFIG_HOME is set (always true for the harness).
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
  // A runner that read its record just before the rename can save it back.
  const revoked = new Map<string, { pid: number; generation: string }>();
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
    if (!runner) {
      if (processes.some((p) => p.group === pid)) groups.add(pid);
      continue;
    }
    const prefix = `${executable} __acp-runner --socket ${socketPath} --session-id ${sessionId} `;
    if (
      runner.group !== pid ||
      !runner.command.startsWith(prefix) ||
      !runner.command.split(" -- ")[0].endsWith(` --generation ${generation}`)
    ) {
      throw new Error(`runner ${pid} no longer matches ${name}; retaining ${appDir}`);
    }
    revoked.set(recordName, { pid, generation });
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
    for (const [recordName, owner] of revoked) revokeAgain(join(workersDir, recordName), owner);
    await delay(50);
  }
}

/** Rename a record the runner saved back after revocation, if it still names that runner. */
function revokeAgain(path: string, owner: { pid: number; generation: string }): void {
  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
    throw error;
  }
  const { pid } = JSON.parse(raw);
  if (pid !== owner.pid || raw.match(/"generation"\s*:\s*(\d+)/)?.[1] !== owner.generation) return;
  try {
    renameSync(path, `${path}.stopping`);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
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
  // About 14 retries per (worker, parallel) slot before colliding with the next slot.
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
  // The wizard only offers agents whose binary is on PATH.
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
  // Shim every command the supervisor can resolve (claude maps to claude-agent-acp), or it finds a
  // real adapter. Env is re-exported because the daemon to runner chain drops some variables.
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
    // The version gate keys off the binary name, so impersonate that agent's handshake. The native
    // `codex` shim makes the wizard offer codex even though the supervisor spawns `codex-acp`.
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

export async function loginWithPassphrase(
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

  // Canonical so the browse endpoint's home check passes on macOS, and under /tmp so runner
  // socket paths stay within the 104-byte Darwin sun_path limit.
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
    let script = opts.fakeAcpScript;
    if (script !== undefined && typeof script !== "string") {
      const path = fakeAcpScriptPath(home);
      writeFileSync(path, JSON.stringify(script));
      script = path;
    }
    writeFakeAcpShim(
      shimBin,
      script,
      fakeAcpDebugLog,
      typeof opts.extraEnv === "function" ? opts.extraEnv(home) : opts.extraEnv,
    );
  } else {
    writeFakeClaudeShim(shimBin);
  }

  const authMode: AuthMode = opts.authMode ?? "none";

  const seedEnv: NodeJS.ProcessEnv = {
    // The isolated HOME is only isolated if nothing overrides where the agents
    // read their config and data from. See `isolatedEnv.ts`.
    ...isolateEnv(process.env, { home, xdgConfig: xdg, xdgData, tmp, tmuxTmp }),
    PATH: `${shimBin}:${process.env.PATH ?? ""}`,
    // Debug-only: a contended runner under coverage can take over 10s to bind its socket.
    AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS: "60000",
    // Teardown revokes the registry lease and waits for this runner-owned watchdog.
    AOE_ACP_WATCHDOG_POLL_MS: "100",
    FAKE_ACP_DEBUG_LOG: fakeAcpDebugLog,
    // trace adds enough I/O to cause unrelated REST flakes on CI.
    AOE_LOG_LEVEL: process.env.AOE_LOG_LEVEL ?? "info",
    // Suppresses the telemetry consent modal, whose backdrop would intercept every click.
    DO_NOT_TRACK: process.env.DO_NOT_TRACK ?? "1",
    // Debug builds ignore TMUX_TMPDIR, so pin the socket raw tmux calls use.
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
      // `--passphrase` alone keeps token auth with a passphrase second factor, which renders TokenEntryPage.
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
