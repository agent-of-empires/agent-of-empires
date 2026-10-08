import { join } from "node:path";
import { describe, expect, it } from "vitest";

import { isolateEnv, pinnedVars, type IsolatedPaths } from "./isolatedEnv";

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
  AOE_DEFER_SANDBOX_MIGRATION: "1",
  AOE_DAEMON_PASSPHRASE: "host-passphrase",
  AOE_DAEMON_TOKEN: "host-token",
  AOE_DAEMON_URL: "http://a-real-daemon.internal:8080",
  AOE_E2E_INPUT_BARRIER: `${HOST}/input-barrier`,
  AOE_E2E_PARTIAL_FRAME_FILE: `${HOST}/partial-frame`,
  AOE_E2E_PROMPT_COMPLETED_FILE: `${HOST}/prompt-completed`,
  AOE_E2E_STORAGE_LOCK_CONTENDED: `${HOST}/lock-contended`,
  AOE_ISOLATED_RUNNER_TEST: "host-owned-case",
  AOE_TEST_STOP_RETIRE_GATE: `${HOST}/retirement-release`,
  AOE_TEST_NATIVE_ISSUER: JSON.stringify({ release: `${HOST}/native-release`, ready: `${HOST}/native-ready` }),
  AOE_TEST_RESERVATION_RUNTIME_SHUTDOWN: `${HOST}/reservation-shutdown`,
  AOE_TUI_TEST_CHILD: "host-test-child",
  AOE_TUI_TEST_ENTERED: `${HOST}/test-entered`,
  AOE_GITHUB_CLONE_BASE: `file://${HOST}/plugins`,
  AOE_AGENT_BIN: "host-session",
  AOE_AGENT_PID: "host-session",
  AOE_AGENT_PROGRAM: "host-session",
  AOE_INSTANCE_ID: "host-session",
  AOE_OMP_CAPTURE_META: "host-meta",
  AOE_OMP_CAPTURE_READY: "1",
  AOE_OMP_LAUNCH_ID: "host-launch",
  AOE_OPEN_URL_TO: `${HOST}/opened-urls.txt`,
  AOE_SERVE_INSTANCE_ID: "host-daemon",
  AOE_SERVE_PASSPHRASE: "host-secret",
  AOE_SESSION_SOURCE: "0f0f0f0f-0000-4000-8000-000000000000",
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
  });
});
