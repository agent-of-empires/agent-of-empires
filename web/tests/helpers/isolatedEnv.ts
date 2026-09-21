// The live daemon inherits process.env, so any variable naming config, data, or credentials would point it at
// real agent state. Path-shaped names are dropped wholesale, a few needed ones are kept, and host state under
// unsuffixed names is dropped or pinned by name (#3657). isolatedEnv.test.ts enforces the contract against src/.

import { join } from "node:path";

export interface IsolatedPaths {
  home: string;
  xdgConfig: string;
  xdgData: string;
  tmp: string;
  tmuxTmp: string;
}

/** Shape of a variable naming a path: `CODEX_HOME`, `OPENCODE_DB`, `PI_CONFIG_DIR`. */
const PATH_VAR = /^[A-Z][A-Z0-9_]*_(HOME|DIR|DB|PATH|CREDENTIALS)$/;

/** All of git's namespace is host state, mostly without a path suffix, so drop it by prefix. */
const GIT_VAR = /^GIT_/;

/** Host state read under names the rules above cannot see. */
export const HOST_STATE_VARS = new Set([
  // Raises logging to debug when AOE_LOG_LEVEL is unset.
  "AGENT_OF_EMPIRES_DEBUG",
  // Moves the profile dir and config.
  "AGENT_OF_EMPIRES_PROFILE",
  "AOE_ACP_AGENT_ENV", // the daemon -> runner env carrier, decoded into agents
  "AOE_ACP_NODE", // an arbitrary host Node executable for the ACP runner
  "AOE_CITYHALL_MODE", // serves the daemon as a client of a host CityHall
  // Would skip the v027 sandbox store migration.
  "AOE_DEFER_SANDBOX_MIGRATION",
  // Fetched and applied on boot; unreachable aborts the daemon.
  "AOE_CITYHALL_BUNDLE_TOKEN",
  "AOE_CITYHALL_BUNDLE_URL",
  // Discovery must not prefer a host endpoint over the private daemon.
  "AOE_DAEMON_TOKEN",
  "AOE_DAEMON_URL",
  // Private fixture files and subprocess controls cannot come from the host.
  "AOE_E2E_INPUT_BARRIER",
  "AOE_E2E_PARTIAL_FRAME_FILE",
  "AOE_E2E_PROMPT_COMPLETED_FILE",
  "AOE_E2E_STORAGE_LOCK_CONTENDED",
  "AOE_TUI_TEST_CHILD",
  "AOE_TUI_TEST_ENTERED",
  "AOE_GITHUB_CLONE_BASE", // redirects plugin clones at a host path or tree
  "AOE_OPEN_URL_TO", // appends every URL the TUI opens to a host file
  "AOE_SERVE_INSTANCE_ID", // identifies a host daemon process as this one
  "AOE_SERVE_PASSPHRASE", // host credential for the daemon's own auth
  // Host endpoints for the daemon's outbound calls.
  "AOE_TELEMETRY_ENDPOINT",
  "AOE_UPDATE_API_BASE",
  "AOE_UPDATE_BASE_URL",
  // The session the runner was launched from, and its agent process and capture markers.
  "AOE_AGENT_BIN",
  "AOE_AGENT_PID",
  "AOE_CAPTURED_SESSION_ID",
  "AOE_INSTANCE_ID",
  "AOE_OMP_CAPTURE_META",
  "AOE_OMP_CAPTURE_READY",
  "AOE_OMP_LAUNCH_ID",
  "TMUX",
  "TMUX_PANE",
  // Re-pinned to the private socket by spawnAoeServe.
  "AOE_TMUX_SOCKET",
]);

/** Kept path variables: toolchain and system locations. XDG_RUNTIME_DIR names host sockets, not data. */
export const INHERITED_PATH_VARS = new Set([
  "CARGO_HOME",
  "DYLD_FALLBACK_LIBRARY_PATH",
  "DYLD_LIBRARY_PATH",
  "LD_LIBRARY_PATH",
  "RUSTUP_HOME",
  "SSL_CERT_DIR",
  "XDG_RUNTIME_DIR",
]);

/** Pinned rather than dropped, since git falls back to /etc/gitconfig and $HOME/.gitconfig; keeps daemon writes in the test home. */
export function pinnedVars(paths: IsolatedPaths): Record<string, string> {
  return {
    GIT_CONFIG_GLOBAL: join(paths.home, ".gitconfig"),
    GIT_CONFIG_SYSTEM: "/dev/null",
  };
}

/** `parentEnv` with owned bases redirected into the test HOME and other path overrides dropped. */
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
