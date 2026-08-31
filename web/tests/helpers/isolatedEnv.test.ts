import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { INHERITED_PATH_VARS, isolateEnv, type IsolatedPaths } from "./isolatedEnv";

const HOME = "/tmp/aoe-pw-w0-p0-test";
const HOST = "/host/dev";
const PATHS: IsolatedPaths = {
  home: HOME,
  xdgConfig: join(HOME, "config"),
  xdgData: join(HOME, "share"),
  tmp: join(HOME, "tmp"),
  tmuxTmp: join(HOME, "tmux"),
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
    expect(env.LANG).toBe("en_US.UTF-8");
  });

  // The daemon resolves agent paths from its own env, so every new agent adds
  // a way for a live spec to reach host state. Drive the assertion off `src/`
  // rather than off a list here, which would fall behind it.
  it("drops every path variable src/ reads, whatever the name", () => {
    const names = daemonPathVars();
    expect(names).toEqual(expect.arrayContaining(["CLAUDE_CONFIG_DIR", "OPENCODE_DB", "VIBE_HOME", "XDG_DATA_HOME"]));

    const hostile: NodeJS.ProcessEnv = {};
    for (const name of names) hostile[name] = `${HOST}/${name.toLowerCase()}`;
    const env = isolateEnv(hostile, PATHS);

    const survived = Object.entries(env)
      .filter(([, value]) => value?.startsWith(HOST))
      .map(([name]) => name);
    expect(survived.sort()).toEqual(names.filter((name) => INHERITED_PATH_VARS.has(name)).sort());
  });
});

/** Every variable naming a path that `src/` mentions, e.g. `KIMI_CODE_HOME`. */
function daemonPathVars(): string[] {
  const srcDir = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "src");
  const names = new Set<string>();
  for (const file of rustFiles(srcDir)) {
    for (const [, name] of readFileSync(file, "utf8").matchAll(
      /"([A-Z][A-Z0-9_]*_(?:HOME|DIR|DB|PATH|CREDENTIALS))"/g,
    )) {
      if (name) names.add(name);
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
