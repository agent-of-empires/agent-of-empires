// Live-backend test harness for Playwright.
//
// `spawnAoeServe()` boots a real `aoe serve` subprocess against an isolated
// filesystem root (`HOME`, `XDG_CONFIG_HOME`, `TMPDIR`, `TMUX_TMPDIR`) and a
// per-worker port range, returns a `ServeHandle`, and cleans up after the
// test via `stop()`. Designed for fresh-process-per-test isolation: each
// test gets its own root, its own port, its own tmux socket.
//
// Worker isolation: callers pass `workerIndex` and `parallelIndex` (from
// Playwright's `testInfo`). Port and TMUX_TMPDIR are derived deterministically
// so parallel workers never collide. tmux is contained inside the test's
// HOME tree, so cleanup is a simple `rm -rf home`.
//
// See `docs/development/playwright.md` for the full recipe.

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  writeFileSync,
  chmodSync,
  mkdirSync,
  realpathSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { randomBytes } from "node:crypto";

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
   * pre-authed browser context (e.g. a future cockpit-under-passphrase
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
   * shim instead of the tail-f-dev-null stub, and flip the cockpit
   * master enable flag via `PATCH /api/cockpit/master` after the server
   * boots.
   */
  cockpit?: boolean;
  /** Optional path to a FAKE_ACP_SCRIPT for cockpit tests. */
  fakeAcpScript?: string;
  /** Extra environment variables exported in the fake-ACP shim. Lets
   *  cockpit tests toggle behavior on the fake agent (e.g. force a
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
  stop(): Promise<void>;
  /**
   * Kill the running `aoe serve` proc and respawn it with the same args
   * on the same port. Used by connectivity-recovery specs (disconnect
   * banner) that need to observe the dashboard's `setServerDown(true)`
   * path on SIGTERM and then `setServerDown(false)` once the server is
   * back. The captured port is reused after the dead listener releases
   * it on `exit`. Token-mode reads the freshly written `serve.token`
   * and updates `handle.authToken`. Does NOT re-run passphrase
   * `preloginViaHarness` or cockpit master enable; specs that need
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
  throw new Error(
    `GET /api/sessions returned an unexpected shape: ${JSON.stringify(body).slice(0, 200)}`,
  );
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
}): (seedEnv: {
  home: string;
  shimBin: string;
  env: NodeJS.ProcessEnv;
}) => void {
  return ({ home, env }) => {
    const projectDir = join(home, opts.subdir ?? "project");
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
    const addRes = spawnSync(
      resolveAoeBinary(),
      ["add", projectDir, "-t", opts.title, "-c", opts.tool ?? "claude"],
      { env },
    );
    if (addRes.status !== 0) {
      throw new Error(
        `aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`,
      );
    }
  };
}

export function resolveAoeBinary(): string {
  const fromEnv = process.env.AOE_E2E_BINARY;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  const repoRoot = resolve(__dirname, "..", "..", "..");
  // Prefer release if both exist (CI builds release by default), fall
  // back to debug for local `cargo build` flows.
  const release = join(repoRoot, "target", "release", "aoe");
  if (existsSync(release)) return release;
  return join(repoRoot, "target", "debug", "aoe");
}

/**
 * Map a resolved aoe binary path to the tmux session prefix the binary
 * will use. The Rust side sets the prefix at compile time based on
 * `cfg!(debug_assertions)`; we can't query it from JS, so we derive it
 * from the build directory in the path. CI passes the binary via
 * `AOE_E2E_BINARY` so this works in CI; locally it falls through to the
 * release/debug heuristic in `resolveAoeBinary`.
 */
export function tmuxPrefixFor(binaryPath: string): "aoe_" | "aoe_dev_" {
  return binaryPath.includes("/target/debug/") ? "aoe_dev_" : "aoe_";
}

/**
 * Resolve where the daemon will write `serve.token` (and other serve.*
 * state files) under the test's isolated filesystem tree. Mirrors the
 * Rust `get_app_dir_path` logic at `src/session/mod.rs:83`: Linux uses
 * `$XDG_CONFIG_HOME/agent-of-empires[-dev]`, macOS/Windows uses
 * `$HOME/.agent-of-empires[-dev]`. Debug builds carry the `-dev`
 * suffix, derived from the binary path the same way as `tmuxPrefixFor`.
 */
export function appDirFor(home: string, xdg: string, binaryPath: string): string {
  const suffix = binaryPath.includes("/target/debug/") ? "-dev" : "";
  if (process.platform === "linux") {
    return join(xdg, `agent-of-empires${suffix}`);
  }
  return join(home, `.agent-of-empires${suffix}`);
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
): Promise<void> {
  const deadline = Date.now() + deadlineMs;
  let lastErr: unknown = "no attempts made";
  while (Date.now() < deadline) {
    if (proc.exitCode !== null || proc.signalCode !== null) {
      throw new Error(
        `aoe serve died before ready (exit=${proc.exitCode} signal=${proc.signalCode})`,
      );
    }
    try {
      const res = await fetch(`${baseUrl}/api/about`);
      // In `--no-auth` mode the server returns 200 outright. In passphrase
      // mode it returns 401 BUT also sets a distinct WWW-Authenticate-ish
      // response shape. Accepting 401 here without distinguishing makes
      // the harness latch onto stale token-auth servers that other test
      // runs left running on the same port. Be precise per authMode.
      if (authMode === "none" && res.status === 200) return;
      if (
        (authMode === "passphrase" || authMode === "token") &&
        (res.status === 200 || res.status === 401)
      )
        return;
      lastErr = `status ${res.status}`;
    } catch (err) {
      lastErr = err;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`aoe serve at ${baseUrl} not ready: ${lastErr}`);
}

function writeFakeClaudeShim(binDir: string): void {
  // Dashboard tracer specs only need the tmux pane to stay open with a
  // long-running process. Cockpit specs swap this for the ACP agent shim
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
  // The cockpit supervisor resolves the agent through `AgentRegistry`
  // (src/cockpit/agent_registry.rs): the `claude` tool key maps to
  // command `claude-agent-acp`, not `claude`. `resolve_agent_command`
  // walks $PATH and node-version dirs, so without a `claude-agent-acp`
  // entry in the shim dir the supervisor falls through to the real
  // installed adapter, which then surfaces "Authentication required"
  // on the first prompt. Shim every name a cockpit test can land on.
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
  const script = `#!/bin/bash\n${scriptLines.join("\n")}\nexec node ${JSON.stringify(fakeAgentJs)} "$@"\n`;
  for (const name of ["claude", "claude-agent-acp", "aoe-agent", "opencode"]) {
    const path = join(binDir, name);
    writeFileSync(path, script);
    chmodSync(path, 0o755);
  }
}

async function enableCockpitMaster(
  baseUrl: string,
  sessionCookie?: { name: string; value: string },
): Promise<void> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (sessionCookie) {
    // Required when authMode === "passphrase"; loopback bypass kicks in
    // for token+loopback callers, but PATCH /api/cockpit/master predates
    // any browser navigation, so the SPA hasn't yet seeded the cookie
    // into a Playwright context. Include it on the harness's own request.
    headers.Cookie = `${sessionCookie.name}=${sessionCookie.value}`;
  }
  const res = await fetch(`${baseUrl}/api/cockpit/master`, {
    method: "PATCH",
    headers,
    body: JSON.stringify({ enabled: true }),
  });
  if (!res.ok) {
    throw new Error(
      `PATCH /api/cockpit/master failed: ${res.status} ${await res.text()}`,
    );
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
    body: JSON.stringify({ passphrase, device_binding_secret: deviceBindingSecret }),
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
      `aoe binary not found at ${aoeBinary}. ` +
        `Set AOE_E2E_BINARY or run liveGlobalSetup.ts to build it.`,
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
  // chars). After we append `/.agent-of-empires-dev/cockpit-workers/
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
  const home = realpathSync(
    mkdtempSync(join(shortBase, `aoe-pw-w${opts.workerIndex}-p${opts.parallelIndex}-`)),
  );
  const xdg = join(home, "config");
  const tmp = join(home, "tmp");
  const tmuxTmp = join(home, "tmux");
  const shimBin = join(home, "bin");
  for (const dir of [xdg, tmp, tmuxTmp, shimBin]) {
    mkdirSync(dir, { recursive: true, mode: 0o700 });
  }
  const fakeAcpDebugLog = join(home, "fake-acp.log");
  if (opts.cockpit) {
    writeFakeAcpShim(shimBin, opts.fakeAcpScript, fakeAcpDebugLog, opts.extraEnv);
  } else {
    writeFakeClaudeShim(shimBin);
  }

  const authMode: AuthMode = opts.authMode ?? "none";

  const seedEnv: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: home,
    XDG_CONFIG_HOME: xdg,
    TMPDIR: tmp,
    TMUX_TMPDIR: tmuxTmp,
    PATH: `${shimBin}:${process.env.PATH ?? ""}`,
    // Lift the runner-socket appearance deadline. The `aoe
    // __cockpit-runner` shim re-execs the debug `aoe` binary, which
    // under v8 coverage + 3 parallel workers + tmux + a fake-ACP node
    // subprocess can take >10s to bind its unix listener on a
    // contended runner. The production 10s default in
    // `runner_socket_deadline()` covers cold caches; tests need
    // headroom or `cockpit_enable` fails with `runner socket … did
    // not appear within 10s` (deterministic on slower local + CI
    // machines, never on hot caches). Honored only in debug builds.
    AOE_COCKPIT_RUNNER_SOCKET_TIMEOUT_MS: "60000",
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
  };

  if (authMode === "token") {
    if (typeof opts.tokenLifetimeSecs === "number") {
      seedEnv.AOE_TEST_TOKEN_LIFETIME_SECS = String(opts.tokenLifetimeSecs);
    }
    if (typeof opts.tokenGraceSecs === "number") {
      seedEnv.AOE_TEST_TOKEN_GRACE_SECS = String(opts.tokenGraceSecs);
    }
  }

  if (opts.seedFn) {
    await opts.seedFn({ home, shimBin, xdg, tmp, tmuxTmp, env: seedEnv });
  }

  const passphrase = authMode === "passphrase" ? opts.passphrase ?? DEFAULT_PASSPHRASE : undefined;

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

  async function spawnOnce(
    args: string[],
    boundBaseUrl: string,
  ): Promise<ChildProcess> {
    const child = spawn(aoeBinary, args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: seedEnv,
    });

    if (process.env.AOE_E2E_DEBUG === "1") {
      // Append per-worker server stdio + spawn env to a fixed path so
      // CI runs can post-mortem failures without holding open pipes
      // that buffer-fill and stall the harness.
      const logPath = `/tmp/aoe-e2e-debug-${opts.workerIndex}-${opts.parallelIndex}.log`;
      const fs = await import("node:fs");
      const log = fs.createWriteStream(logPath, { flags: "a" });
      log.write(`\n=== spawn ${args.join(" ")} (home=${home}) ===\n`);
      child.stdout?.on("data", (b) => log.write(`[stdout] ${b}`));
      child.stderr?.on("data", (b) => log.write(`[stderr] ${b}`));
    }

    let spawnFailed = false;
    child.once("error", () => {
      spawnFailed = true;
    });

    try {
      await waitForServer(boundBaseUrl, spawnTimeoutMs, child, authMode);
      return child;
    } catch (err) {
      try {
        child.kill("SIGKILL");
      } catch {
        // ignore
      }
      const wrapped = spawnFailed
        ? new Error(`spawn failed before listen: ${String(err)}`)
        : err;
      throw wrapped;
    }
  }

  let proc: ChildProcess | null = null;
  let port = 0;
  let baseUrl = "";

  for (let attempt = 0; attempt < 5; attempt++) {
    port = portFor(opts.workerIndex, opts.parallelIndex, attempt);
    baseUrl = `http://127.0.0.1:${port}`;
    try {
      proc = await spawnOnce(buildArgs(port), baseUrl);
      break;
    } catch (err) {
      if (attempt === 4) {
        rmSync(home, { recursive: true, force: true });
        throw err;
      }
      // try next port
    }
  }

  if (!proc) {
    rmSync(home, { recursive: true, force: true });
    throw new Error("aoe serve failed to bind on every attempted port");
  }

  let authToken: string | undefined;
  let tokenFile: string | undefined;
  if (authMode === "token") {
    tokenFile = join(appDirFor(home, xdg, aoeBinary), "serve.token");
    authToken = await readTokenFile(tokenFile, spawnTimeoutMs);
  }

  async function killProc(child: ChildProcess): Promise<void> {
    if (child.exitCode !== null || child.signalCode !== null) return;
    child.kill("SIGTERM");
    await new Promise<void>((resolveExit) => {
      let resolved = false;
      const done = () => {
        if (resolved) return;
        resolved = true;
        clearTimeout(escalate);
        clearTimeout(backstop);
        resolveExit();
      };
      // 2s after SIGTERM, escalate to SIGKILL. Do NOT resolve here:
      // restart() reuses the same port and a too-early resolve races
      // the kernel's TCP cleanup, so spawnOnce can land on EADDRINUSE.
      // Wait for the real exit event (or the backstop below).
      const escalate = setTimeout(() => {
        try {
          child.kill("SIGKILL");
        } catch {
          // ignore
        }
      }, 2000);
      // Hard backstop so a pathologically uncooperative child can't
      // hang the test forever. SIGKILL is uninterruptible on POSIX
      // outside zombie/D-state, so this should not fire in practice.
      const backstop = setTimeout(done, 4000);
      child.once("exit", done);
    });
  }

  const handle: ServeHandle = {
    baseUrl,
    port,
    home,
    shimBin,
    proc,
    authMode,
    passphrase,
    authToken,
    tokenFile,
    tmuxPrefix: tmuxPrefixFor(aoeBinary),
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
    async stop() {
      try {
        if (proc) await killProc(proc);
      } finally {
        // Best-effort: kill any tmux server bound to the isolated socket
        // before deleting the dir. Cockpit specs leave tmux child
        // processes around that hold open file descriptors and trip
        // ENOTEMPTY on rmSync if not cleaned up first.
        try {
          spawnSync("tmux", ["kill-server"], {
            env: {
              ...process.env,
              HOME: home,
              TMUX_TMPDIR: join(home, "tmux"),
            },
            stdio: "ignore",
          });
        } catch {
          // tmux not installed or no server running; either way we don't
          // care.
        }
        // Removing the home dir wipes the isolated TMUX_TMPDIR socket too.
        // Wrap in try/catch: stale fds, slow umount, or AFS-style retry
        // semantics can leave non-empty dirs that don't matter for the
        // test result.
        try {
          rmSync(home, { recursive: true, force: true });
        } catch {
          // best effort
        }
      }
    },
  };

  if (authMode === "passphrase" && passphrase && opts.preloginViaHarness) {
    const deviceBindingSecret = randomBytes(32).toString("base64url");
    const { cookie } = await loginWithPassphrase(baseUrl, passphrase, deviceBindingSecret);
    handle.sessionCookie = cookie;
    handle.deviceBindingSecret = deviceBindingSecret;
  }

  if (opts.cockpit) {
    await enableCockpitMaster(baseUrl, handle.sessionCookie);
  }

  return handle;
}
