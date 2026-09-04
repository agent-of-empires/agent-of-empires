import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { HOST_STATE_VARS, INHERITED_PATH_VARS, isolateEnv, pinnedVars, type IsolatedPaths } from "./isolatedEnv";

const HOME = "/tmp/aoe-pw-w0-p0-test";
const HOST = "/host/dev";
const PATHS: IsolatedPaths = {
  home: HOME,
  xdgConfig: join(HOME, "config"),
  xdgData: join(HOME, "share"),
  tmp: join(HOME, "tmp"),
  tmuxTmp: join(HOME, "tmux"),
};

/**
 * Host state the daemon reads under a name that carries no path suffix, with
 * a hostile value for each. Written out here rather than derived from `src/`
 * or from the implementation's own rule: a scanner keyed on that rule cannot
 * see a name the rule was never written for, which is how these survived the
 * first source-driven test (#3657).
 */
const NON_SUFFIX_HOST_STATE: Record<string, string> = {
  AGENT_OF_EMPIRES_DEBUG: "1",
  AGENT_OF_EMPIRES_PROFILE: "work",
  AOE_ACP_AGENT_ENV: '[["ANTHROPIC_API_KEY","host-key"]]',
  AOE_ACP_NODE: `${HOST}/.nvm/versions/node/v22.0.0/bin/node`,
  AOE_CAPTURED_SESSION_ID: "host-captured",
  AOE_CITYHALL_BUNDLE_TOKEN: "host-bundle-token",
  AOE_CITYHALL_BUNDLE_URL: "https://host.invalid/bundle",
  AOE_CITYHALL_MODE: "1",
  AOE_DAEMON_TOKEN: "host-token",
  AOE_DAEMON_URL: "http://a-real-daemon.internal:8080",
  AOE_GITHUB_CLONE_BASE: `file://${HOST}/plugins`,
  AOE_INSTANCE_ID: "host-session",
  AOE_OMP_CAPTURE_META: "host-meta",
  AOE_OMP_CAPTURE_READY: "1",
  AOE_OMP_LAUNCH_ID: "host-launch",
  AOE_OPEN_URL_TO: `${HOST}/opened-urls.txt`,
  AOE_SERVE_INSTANCE_ID: "host-daemon",
  AOE_SERVE_PASSPHRASE: "host-secret",
  AOE_TELEMETRY_ENDPOINT: "https://host.invalid/v1/telemetry",
  AOE_TMUX_SOCKET: "/tmp/tmux-1000/aoe.sock",
  AOE_UPDATE_API_BASE: "https://host.invalid/api",
  AOE_UPDATE_BASE_URL: "https://host.invalid",
  GIT_CONFIG_GLOBAL: `${HOST}/.gitconfig`,
  GIT_CONFIG_SYSTEM: `${HOST}/etc/gitconfig`,
  GIT_SSH_COMMAND: `ssh -i ${HOST}/.ssh/id_ed25519`,
  GIT_WORK_TREE: `${HOST}/repo`,
  TMUX: "/tmp/tmux-1000/default,4242,0",
  TMUX_PANE: "%7",
};

/**
 * Variables `src/` reads that the daemon inherits on purpose, grouped by why.
 * A name that is neither here nor neutralized fails the contract test below,
 * which is the point: classifying a new variable stays a decision someone
 * makes, instead of one the suffix rule makes for them.
 */
const INHERITED_BY_CONTRACT = new Set([
  ...INHERITED_PATH_VARS,
  // Timing, tracing, and test switches. Their values are numbers or flags, so
  // a hostile one can slow a spec down but cannot point the daemon anywhere.
  "AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS",
  "AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT",
  "AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES",
  "AOE_ACP_TRACE",
  "AOE_ACP_WATCHDOG_POLL_MS",
  "AOE_E2E_DEBUG",
  "AOE_ENVGUARD_DUP",
  "AOE_ENVGUARD_EMPTY",
  "AOE_ENVGUARD_UNSET",
  "AOE_ENVGUARD_UNSET_ME",
  "AOE_FILE_WATCH",
  "AOE_LOG_LEVEL",
  "AOE_MOUSE_CAPTURE",
  "AOE_NO_LNAV_TIP",
  "AOE_PRIVDROP_TESTS",
  "AOE_RECOVERY_HOOK_TIMEOUT_MS",
  "AOE_RESUME_IDLE_GRACE_MS",
  "AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS",
  "AOE_SILENT_ORPHAN_FAST_GRACE_MS",
  "AOE_SILENT_ORPHAN_GRACE_MS",
  "AOE_TERMINAL_TRACE",
  "AOE_TEST_TOKEN_GRACE_SECS",
  "AOE_TEST_TOKEN_LIFETIME_SECS",
  // A marker `aoe` echoes into a pane to probe a login shell, not a variable
  // the daemon resolves anything from.
  "AOE_AGENT_OK",
  // Reaches a real agent binary, never the daemon's own state, and every live
  // spec runs a fake agent shim.
  "CLAUDE_CODE_USE_VERTEX",
  // Terminal, desktop, and remote-session hints the daemon forwards into the
  // sessions it creates.
  "DISPLAY",
  "DO_NOT_TRACK",
  "MOSH_CONNECTION",
  "NO_COLOR",
  "SSH_CLIENT",
  "SSH_CONNECTION",
  "SSH_TTY",
  "USER",
  "WAYLAND_DISPLAY",
  // Host executables the daemon runs on the user's behalf: `user_shell()`
  // wraps pane commands in `$SHELL` so rc files load, and the TUI opens
  // `$EDITOR`. Inherited on purpose, so a spec exercises the same programs
  // the developer has; `spawnAoeServe` prepends its shim dir to PATH.
  "EDITOR",
  "PATH",
  "SHELL",
  "VISUAL",
]);

describe("isolateEnv", () => {
  // #3622: XDG_DATA_HOME and OPENCODE_DB reached the daemon, which reads both
  // to find opencode's session database.
  it("leaves no inherited path pointing outside the test home", () => {
    const env = isolateEnv(
      {
        HOME: HOST,
        XDG_CONFIG_HOME: `${HOST}/.config`,
        XDG_DATA_HOME: `${HOST}/.local/share`,
        OPENCODE_DB: `${HOST}/.local/share/opencode/opencode.db`,
        CLAUDE_CONFIG_DIR: `${HOST}/.claude`,
        GIT_EXEC_PATH: `${HOST}/libexec/git-core`,
        AOE_DAEMON_URL: "http://a-real-daemon.internal:8080",
        TMPDIR: `${HOST}/tmp`,
        LANG: "en_US.UTF-8",
      },
      PATHS,
    );

    for (const [name, value] of Object.entries(env)) {
      expect(`${name}=${value}`).not.toContain(HOST);
    }
    expect(env.XDG_DATA_HOME).toBe(PATHS.xdgData);
    // Not a path, but it would point the harness's own `aoe` calls at it.
    expect(env.AOE_DAEMON_URL).toBeUndefined();
    // Not inherited as a toolchain path: git resolves the subprograms it runs
    // from it, and finds them on its own once it is gone.
    expect(env.GIT_EXEC_PATH).toBeUndefined();
    expect(env.LANG).toBe("en_US.UTF-8");
  });

  // #3657: the suffix rule missed GIT_CONFIG_*, AOE_ACP_NODE and
  // AOE_GITHUB_CLONE_BASE, so host git config, a host Node binary, and a host
  // plugin source still reached `aoe serve`.
  it("neutralizes host state whose name carries no path suffix", () => {
    const pinned = pinnedVars(PATHS);
    const env = isolateEnv({ HOME: HOST, ...NON_SUFFIX_HOST_STATE }, PATHS);

    for (const name of Object.keys(NON_SUFFIX_HOST_STATE)) {
      expect(env[name], `${name} reached the daemon unchanged`).toBe(pinned[name]);
    }
    expect(env.GIT_CONFIG_GLOBAL).toBe(join(HOME, ".gitconfig"));
    expect(env.GIT_CONFIG_SYSTEM).toBe("/dev/null");

    // Pinned unconditionally: an unset GIT_CONFIG_SYSTEM still leaves the
    // daemon's git reading /etc/gitconfig.
    expect(isolateEnv({}, PATHS)).toMatchObject(pinned);

    // Every name dropped by name carries a case above, so the list cannot
    // grow one that nothing exercises.
    expect(
      [...HOST_STATE_VARS].filter((name) => !(name in NON_SUFFIX_HOST_STATE)),
      "dropped by isolatedEnv.ts with no hostile case here",
    ).toEqual([]);
  });

  // The daemon resolves paths, executables, and endpoints from its own env, so
  // every variable `src/` reads is a way for a live spec to reach host state.
  // Drive the assertion off `src/` and make each name a decision: neutralized
  // by `isolateEnv`, or listed in INHERITED_BY_CONTRACT with the reason.
  it("classifies every environment variable src/ reads", () => {
    const names = daemonEnvVars();
    expect(names).toEqual(
      expect.arrayContaining([
        "AOE_ACP_NODE",
        "AOE_CITYHALL_BUNDLE_URL",
        "AOE_GITHUB_CLONE_BASE",
        "CLAUDE_CONFIG_DIR",
        "OPENCODE_DB",
        "VIBE_HOME",
      ]),
    );

    const hostile: NodeJS.ProcessEnv = {};
    for (const name of names) hostile[name] = `${HOST}/${name.toLowerCase()}`;
    const env = isolateEnv(hostile, PATHS);
    const survived = names.filter((name) => env[name]?.startsWith(HOST));

    expect(
      survived.filter((name) => !INHERITED_BY_CONTRACT.has(name)),
      "src/ reads these and the daemon still gets the host value: neutralize them in isolatedEnv.ts, or add them to INHERITED_BY_CONTRACT with the reason they are safe",
    ).toEqual([]);
    expect(
      names.filter((name) => INHERITED_BY_CONTRACT.has(name) && !survived.includes(name)),
      "the contract says the daemon needs these, but isolateEnv drops them",
    ).toEqual([]);
  });
});

/**
 * Every environment variable `src/` reads: `env::var` / `env::var_os` calls,
 * clap's `env = "..."` attributes, any quoted path-shaped name, which catches
 * the ones the daemon forwards to an agent rather than reads itself, and the
 * `const NAME: &str = "AOE_..."` declarations the call-site patterns cannot
 * see, since `env::var(SOME_ENV)` names the constant and not the variable.
 */
function daemonEnvVars(): string[] {
  const srcDir = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "src");
  const patterns = [
    /env::var(?:_os)?\(\s*"([A-Z][A-Z0-9_]*)"/g,
    /\benv\s*=\s*"([A-Z][A-Z0-9_]*)"/g,
    /"([A-Z][A-Z0-9_]*_(?:HOME|DIR|DB|PATH|CREDENTIALS))"/g,
    /const\s+[A-Z0-9_]+\s*:\s*&(?:'static\s+)?str\s*=\s*"([A-Z][A-Z0-9_]*)"/g,
  ];
  const names = new Set<string>();
  for (const file of rustFiles(srcDir)) {
    const source = readFileSync(file, "utf8");
    for (const pattern of patterns) {
      for (const [, name] of source.matchAll(pattern)) {
        if (name) names.add(name);
      }
    }
  }
  return [...names].sort();
}

function rustFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return rustFiles(path);
    return entry.name.endsWith(".rs") ? [path] : [];
  });
}
