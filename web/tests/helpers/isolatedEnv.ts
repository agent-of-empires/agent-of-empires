// Environment isolation for the live harness.
//
// `spawnAoeServe` starts from `process.env` so the daemon inherits PATH,
// locale, and proxy settings. Anything inherited that names a config, data,
// or credential location escapes the temporary HOME, because the daemon
// resolves agent state from its own environment (`resolve_agent_home` in
// `src/session/capture.rs`, the opencode readers, the XDG bases). A developer
// or CI shell exporting one of them points a live spec at real agent state.
//
// Listing those names does not hold: `src/` reads more than a dozen and gains
// one per agent integration, and a missed name leaks silently. So anything
// shaped like a path override is dropped, and the few the child genuinely
// needs are named instead. Dropping one of those breaks the run loudly, which
// is the safe direction to fail in.

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
 * Not path shaped, but `discovery::discover()` prefers them over the local
 * daemon, so every `aoe` call the harness makes with this env, teardown's
 * `acp stop --all` included, would hit the developer's own daemon and kill
 * its workers.
 */
const DAEMON_TARGET_VARS = new Set(["AOE_DAEMON_TOKEN", "AOE_DAEMON_URL"]);

/**
 * Path variables the child keeps: toolchain and system locations, never agent
 * state. `XDG_RUNTIME_DIR` is the one XDG base not redirected, because it
 * names the host's session sockets rather than a data tree.
 */
export const INHERITED_PATH_VARS = new Set([
  "CARGO_HOME",
  "DYLD_FALLBACK_LIBRARY_PATH",
  "DYLD_LIBRARY_PATH",
  "GIT_EXEC_PATH",
  "LD_LIBRARY_PATH",
  "RUSTUP_HOME",
  "SSL_CERT_DIR",
  "XDG_RUNTIME_DIR",
]);

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
    if (DAEMON_TARGET_VARS.has(name)) continue;
    if (PATH_VAR.test(name) && !INHERITED_PATH_VARS.has(name)) continue;
    env[name] = value;
  }
  return {
    ...env,
    HOME: paths.home,
    XDG_CONFIG_HOME: paths.xdgConfig,
    XDG_DATA_HOME: paths.xdgData,
    TMPDIR: paths.tmp,
    TMUX_TMPDIR: paths.tmuxTmp,
  };
}
