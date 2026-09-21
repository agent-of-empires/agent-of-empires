import { describe, expect, it } from "vitest";
import {
  DEFAULT_AGENT_PROFILE,
  isClearAlias,
  isSubagentToolName,
  resolveAgentLifecycle,
  resolveAgentProfile,
} from "./agentProfiles";

const KNOWN = [
  "claude",
  "claude-code",
  "codex",
  "opencode",
  "gemini",
  "vibe",
  "pi",
  "omp",
  "kimi",
  "prime-agent",
  "aoe-agent",
];

describe("resolveAgentProfile", () => {
  it.each(KNOWN)("resolves %s", (key) => {
    expect(resolveAgentProfile(key).key).toBe(key);
  });

  it.each([undefined, null, "", "custom"])("falls back to DEFAULT for %j", (key) => {
    expect(resolveAgentProfile(key).key).toBe(DEFAULT_AGENT_PROFILE.key);
  });

  it.each<[string, boolean, boolean, boolean, string[]]>([
    ["claude", true, true, true, ["claudeCode"]],
    ["codex", false, false, false, []],
    ["gemini", false, false, false, []],
    ["opencode", true, false, false, []],
    ["omp", false, false, false, []],
  ])("%s capabilities: todos=%s skills=%s wakeup=%s", (key, todos, skills, wakeup, namespaces) => {
    const p = resolveAgentProfile(key);
    expect(p.capabilities).toMatchObject({ todos, skills, wakeup });
    expect(p.parentMetaNamespaces).toEqual(namespaces);
  });

  it("omp and aoe-agent claim no guessed specials", () => {
    expect(resolveAgentProfile("omp").capabilities).toMatchObject({ subagents: false, legacyModeFallback: false });
    const p = resolveAgentProfile("aoe-agent");
    expect(p.capabilities).toEqual({
      todos: false,
      skills: false,
      wakeup: false,
      subagents: false,
      legacyModeFallback: false,
      heartbeatKeepalives: false,
    });
    expect(p.parentMetaNamespaces).toEqual([]);
    expect(p.specialTitles).toEqual({ skillNames: [], scheduleNames: [], harnessNames: [] });
  });

  it("maps agent tool names to canonical cards", () => {
    const codex = resolveAgentProfile("codex").aliases;
    expect([codex.execute, codex.edit]).toEqual([["shell", "bash"], ["apply_patch"]]);
    expect(codex.read).toContain("view_file");
    const opencode = resolveAgentProfile("opencode");
    expect(opencode.aliases).toMatchObject({
      execute: ["bash"],
      edit: ["edit", "write"],
      search: ["grep", "glob"],
      fetch: ["webfetch"],
    });
    expect(opencode.aliases.think).toBeUndefined();
    expect(opencode.subagentToolNames).toEqual(["task"]);
    const gemini = resolveAgentProfile("gemini").aliases;
    expect([gemini.execute, gemini.fetch]).toEqual([["run_shell_command"], ["web_fetch"]]);
    expect(gemini.read).toEqual(expect.arrayContaining(["read_file", "read_many_files"]));
  });
});

describe("resolveAgentLifecycle", () => {
  it("marks gemini deprecated with the antigravity replacement, mirrored on its profile", () => {
    const lifecycle = resolveAgentLifecycle("gemini");
    expect(lifecycle).toMatchObject({
      state: "deprecated",
      since: "2026-06-18",
      replacement: "antigravity",
      note: expect.stringContaining("consumer accounts cut off by Google"),
    });
    expect(resolveAgentProfile("gemini").lifecycle).toEqual(lifecycle);
    expect(DEFAULT_AGENT_PROFILE.lifecycle).toBeUndefined();
  });

  it.each([...KNOWN.filter((k) => k !== "gemini" && k !== "prime-agent"), undefined, null, "", "custom-agent"])(
    "resolves %j as active",
    (key) => {
      expect(resolveAgentLifecycle(key)).toEqual({ state: "active" });
    },
  );
});

it.each<[string, string[], boolean]>([
  ["/clear", ["/clear"], true],
  ["/new", ["/new"], true],
  ["  /clear  ", ["/clear"], true],
  ["\n/clear\n", ["/clear"], true],
  ["/clear --hard", ["/clear"], true],
  ["/new fresh session", ["/new"], true],
  ["clear", ["/clear"], false],
  ["/cleart", ["/clear"], false],
  ["hello /clear world", ["/clear"], false],
  ["", ["/clear"], false],
  ["   ", ["/clear"], false],
  ["/clear", [], false],
  ["/new", ["/clear"], false],
  ["/clear", ["/new"], false],
])("isClearAlias(%j, %j) is %s", (text, aliases, expected) => {
  expect(isClearAlias(text, aliases)).toBe(expected);
});

it.each<[string | null | undefined, string, boolean]>([
  ["task", "opencode", true],
  ["bash", "opencode", false],
  ["task", "codex", false],
  ["task", "claude", false],
  ["task", "aoe-agent", false],
  [undefined, "opencode", false],
  [null, "opencode", false],
  ["", "opencode", false],
])("isSubagentToolName(%j, %s) is %s", (name, agent, expected) => {
  expect(isSubagentToolName(name, resolveAgentProfile(agent))).toBe(expected);
});
