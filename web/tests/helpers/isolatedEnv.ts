// Environment isolation for the live harness.
//
// `spawnAoeServe` starts from `process.env` so the daemon inherits PATH,
// locale, and proxy settings. Anything inherited that names a config, data,
// or credential location escapes the temporary HOME, because the daemon
// resolves agent state from its own environment (`resolve_agent_home` in
// `src/session/capture.rs`, the opencode readers, the XDG bases). A developer
// or CI shell exporting one of them points a live spec at real agent state.
//
// Listing every such name does not hold on its own: `src/` reads more than a
// dozen and gains one per agent integration, and a missed name leaks
// silently. So anything shaped like a path override is dropped, and the few
// the child genuinely needs are named instead. Dropping one of those breaks
// the run loudly, which is the safe direction to fail in.
//
// The shape rule alone is not enough either: `GIT_CONFIG_GLOBAL`,
// `AOE_ACP_NODE` and `AGENT_OF_EMPIRES_PROFILE` name host state under no
// suffix at all (#3657). Those are dropped by name, or pinned where dropping
// would only fall back to another host location. `isolatedEnv.test.ts` holds
// the resulting contract and fails on a variable `src/` reads that neither
// rule covers.

import { join } from "node:path";

/** Directories the harness owns, all inside the temporary test HOME. */
export interface IsolatedPaths {
  home: string;
  xdgConfig: string;
  xdgData: string;
  tmp: string;
  tmuxTmp: string;
}

/** Shape of a variable naming a path: `CODEX_HOME`, `OPENCODE_DB`, `PI_CONFIG_DIR`. */
const PATH_VAR = /^[A-Z][A-Z0-9_]*_(HOME|DIR|DB|PATH|CREDENTIALS)$/;

/**
 * Git's whole namespace is host state: config files, work trees, object
 * stores, and the subprograms git resolves and runs, `GIT_EXEC_PATH`
 * included. Most of those names carry no path suffix, so the family is
 * dropped by prefix instead.
 */
const GIT_VAR = /^GIT_/;

/**
 * Host state the daemon reads under a name neither rule above can see. Each
 * entry points `aoe serve`, or an `aoe` call the harness makes with this
 * environment, at host configuration, a host executable, a host repository,
 * or the host session the test runner was launched from.
 */
export const HOST_STATE_VARS = new Set([
  // Raises the daemon to debug logging whenever `AOE_LOG_LEVEL` is unset
  // (`LogConfig::from_env`), adding the log I/O `spawnAoeServe` pins at
  // `info` on purpose.
  "AGENT_OF_EMPIRES_DEBUG",
  // clap's `--profile`, which moves the profile dir and the config the daemon
  // resolves its port and `[tmux]` options from.
  "AGENT_OF_EMPIRES_PROFILE",
  "AOE_ACP_AGENT_ENV", // the daemon -> runner env carrier, decoded into agents
  "AOE_ACP_NODE", // an arbitrary host Node executable for the ACP runner
  "AOE_CITYHALL_MODE", // serves the daemon as a client of a host CityHall
  // `apply_cityhall_bundle` runs on the boot path: a host URL is fetched and
  // applied as config, and a first boot that cannot reach it aborts the
  // daemon outright.
  "AOE_CITYHALL_BUNDLE_TOKEN",
  "AOE_CITYHALL_BUNDLE_URL",
  // `discovery::discover()` prefers these over the local daemon, so every
  // `aoe` call the harness makes with this env, teardown's `acp stop --all`
  // included, would hit the developer's own daemon and kill its workers.
  "AOE_DAEMON_TOKEN",
  "AOE_DAEMON_URL",
  "AOE_GITHUB_CLONE_BASE", // redirects plugin clones at a host path or tree
  "AOE_OPEN_URL_TO", // appends every URL the TUI opens to a host file
  "AOE_SERVE_INSTANCE_ID", // identifies a host daemon process as this one
  "AOE_SERVE_PASSPHRASE", // host credential for the daemon's own auth
  // Host endpoints for the daemon's outbound calls.
  "AOE_TELEMETRY_ENDPOINT",
  "AOE_UPDATE_API_BASE",
  "AOE_UPDATE_BASE_URL",
  // The session the test runner was launched from: `aoe` resolves "the
  // current session" from `TMUX_PANE`, `AOE_INSTANCE_ID` names a host session
  // directly, and the capture markers `aoe` writes into a pane make the
  // daemon read a host launch as its own.
  "AOE_CAPTURED_SESSION_ID",
  "AOE_INSTANCE_ID",
  "AOE_OMP_CAPTURE_META",
  "AOE_OMP_CAPTURE_READY",
  "AOE_OMP_LAUNCH_ID",
  "TMUX",
  "TMUX_PANE",
  // The host tmux server. `spawnAoeServe` re-pins it at `tmuxSocketPath`
  // after this filter, so dropping it only removes the host fallback.
  "AOE_TMUX_SOCKET",
]);

/**
 * Path variables the child keeps: toolchain and system locations, never agent
 * state. `XDG_RUNTIME_DIR` is the one XDG base not redirected, because it
 * names the host's session sockets rather than a data tree.
 */
export const INHERITED_PATH_VARS = new Set([
  "CARGO_HOME",
  "DYLD_FALLBACK_LIBRARY_PATH",
  "DYLD_LIBRARY_PATH",
  "LD_LIBRARY_PATH",
  "RUSTUP_HOME",
  "SSL_CERT_DIR",
  "XDG_RUNTIME_DIR",
]);

/**
 * Variables pinned rather than dropped, because dropping them only falls back
 * to another host location: git reads `/etc/gitconfig` for the system file,
 * and `$HOME/.gitconfig` for the global one. Pinning the global file inside
 * the test home keeps a daemon-side `git config --global` write
 * (`session::cityhall_bundle`) in the tree the harness deletes.
 * `gitFixture.ts` pins both names at `/dev/null` for the fixture
 * subprocesses; this covers the daemon's own git calls.
 */
export function pinnedVars(paths: IsolatedPaths): Record<string, string> {
  return {
    GIT_CONFIG_GLOBAL: join(paths.home, ".gitconfig"),
    GIT_CONFIG_SYSTEM: "/dev/null",
  };
}

/**
 * Copy of `parentEnv` with every agent path pointed inside the test HOME.
 *
 * The bases the harness owns are redirected; the rest are dropped, which
 * leaves the daemon on its `$HOME`-relative fallback (`XDG_STATE_HOME` ->
 * `$HOME/.local/state`, and so on), already inside the test home.
 */
export function isolateEnv(parentEnv: NodeJS.ProcessEnv, paths: IsolatedPaths): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [name, value] of Object.entries(parentEnv)) {
    if (INHERITED_PATH_VARS.has(name)) {
      env[name] = value;
      continue;
    }
    if (HOST_STATE_VARS.has(name)) continue;
    if (PATH_VAR.test(name) || GIT_VAR.test(name)) continue;
    env[name] = value;
  }
  return {
    ...env,
    HOME: paths.home,
    XDG_CONFIG_HOME: paths.xdgConfig,
    XDG_DATA_HOME: paths.xdgData,
    TMPDIR: paths.tmp,
    TMUX_TMPDIR: paths.tmuxTmp,
    ...pinnedVars(paths),
  };
}
