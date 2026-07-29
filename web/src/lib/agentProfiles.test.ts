import { describe, expect, it } from "vitest";
import { DEFAULT_AGENT_PROFILE, isClearAlias, isSubagentToolName, resolveAgentProfile } from "./agentProfiles";

describe("resolveAgentProfile", () => {
  it("resolves known agent keys", () => {
    expect(resolveAgentProfile("claude").key).toBe("claude");
    expect(resolveAgentProfile("claude-code").key).toBe("claude-code");
    expect(resolveAgentProfile("codex").key).toBe("codex");
    expect(resolveAgentProfile("opencode").key).toBe("opencode");
    expect(resolveAgentProfile("gemini").key).toBe("gemini");
    expect(resolveAgentProfile("vibe").key).toBe("vibe");
    expect(resolveAgentProfile("pi").key).toBe("pi");
    expect(resolveAgentProfile("omp").key).toBe("omp");
    expect(resolveAgentProfile("kimi").key).toBe("kimi");
    expect(resolveAgentProfile("aoe-agent").key).toBe("aoe-agent");
  });

  it("falls back to DEFAULT for unknown / nullish keys", () => {
    expect(resolveAgentProfile(undefined).key).toBe(DEFAULT_AGENT_PROFILE.key);
    expect(resolveAgentProfile(null).key).toBe(DEFAULT_AGENT_PROFILE.key);
    expect(resolveAgentProfile("").key).toBe(DEFAULT_AGENT_PROFILE.key);
    expect(resolveAgentProfile("custom").key).toBe(DEFAULT_AGENT_PROFILE.key);
  });

  it("claude has all specialised UI capabilities enabled", () => {
    const p = resolveAgentProfile("claude");
    expect(p.capabilities.todos).toBe(true);
    expect(p.capabilities.skills).toBe(true);
    expect(p.capabilities.wakeup).toBe(true);
    expect(p.parentMetaNamespaces).toEqual(["claudeCode"]);
  });

  it("codex / gemini disable claude-specific cards", () => {
    for (const key of ["codex", "gemini"] as const) {
      const p = resolveAgentProfile(key);
      expect(p.capabilities.todos).toBe(false);
      expect(p.capabilities.skills).toBe(false);
      expect(p.capabilities.wakeup).toBe(false);
      expect(p.parentMetaNamespaces).toEqual([]);
    }
  });

  it("opencode supports todowrite cards but keeps other claude-specific cards disabled", () => {
    const p = resolveAgentProfile("opencode");
    expect(p.capabilities.todos).toBe(true);
    expect(p.capabilities.skills).toBe(false);
    expect(p.capabilities.wakeup).toBe(false);
    expect(p.parentMetaNamespaces).toEqual([]);
  });

  it("omp uses its native ACP clear boundary without guessed capabilities", () => {
    const p = resolveAgentProfile("omp");
    expect(p.clearAliases).toEqual(["/new"]);
    expect(p.capabilities.todos).toBe(false);
    expect(p.capabilities.skills).toBe(false);
    expect(p.capabilities.wakeup).toBe(false);
    expect(p.capabilities.subagents).toBe(false);
    expect(p.capabilities.legacyModeFallback).toBe(false);
    expect(p.parentMetaNamespaces).toEqual([]);
  });

  it("codex aliases route shell / apply_patch / view_file to canonical cards", () => {
    const p = resolveAgentProfile("codex");
    expect(p.aliases.execute).toEqual(["shell", "bash"]);
    expect(p.aliases.edit).toEqual(["apply_patch"]);
    expect(p.aliases.read).toContain("view_file");
  });

  it("opencode aliases cover bash / read / edit / write / grep / glob / webfetch", () => {
    const p = resolveAgentProfile("opencode");
    expect(p.aliases.execute).toEqual(["bash"]);
    expect(p.aliases.edit).toEqual(["edit", "write"]);
    expect(p.aliases.search).toEqual(["grep", "glob"]);
    expect(p.aliases.fetch).toEqual(["webfetch"]);
    // `task` is no longer a think alias; it classifies as a subagent
    // launch by wire name instead. See #3070.
    expect(p.aliases.think).toBeUndefined();
    expect(p.subagentToolNames).toEqual(["task"]);
  });

  it("gemini aliases cover run_shell_command / read_file / web_fetch", () => {
    const p = resolveAgentProfile("gemini");
    expect(p.aliases.execute).toEqual(["run_shell_command"]);
    expect(p.aliases.read).toContain("read_file");
    expect(p.aliases.read).toContain("read_many_files");
    expect(p.aliases.fetch).toEqual(["web_fetch"]);
  });

  it("clearAliases match the server-side rust profile", () => {
    expect(resolveAgentProfile("claude").clearAliases).toEqual(["/clear"]);
    expect(resolveAgentProfile("codex").clearAliases).toEqual(["/new"]);
    expect(resolveAgentProfile("opencode").clearAliases).toEqual(["/new"]);
    expect(resolveAgentProfile("gemini").clearAliases).toEqual([]);
    expect(resolveAgentProfile("kimi").clearAliases).toEqual(["/new"]);
    expect(resolveAgentProfile("omp").clearAliases).toEqual(["/new"]);
  });
});

describe("isClearAlias", () => {
  const claude = ["/clear"];
  const codex = ["/new"];

  it("matches the exact alias", () => {
    expect(isClearAlias("/clear", claude)).toBe(true);
    expect(isClearAlias("/new", codex)).toBe(true);
  });

  it("tolerates surrounding whitespace", () => {
    expect(isClearAlias("  /clear  ", claude)).toBe(true);
    expect(isClearAlias("\n/clear\n", claude)).toBe(true);
  });

  it("matches an invocation with trailing args after a space", () => {
    expect(isClearAlias("/clear --hard", claude)).toBe(true);
    expect(isClearAlias("/new fresh session", codex)).toBe(true);
  });

  it("rejects partial matches and embedded occurrences", () => {
    expect(isClearAlias("clear", claude)).toBe(false);
    expect(isClearAlias("/cleart", claude)).toBe(false);
    expect(isClearAlias("hello /clear world", claude)).toBe(false);
    expect(isClearAlias("", claude)).toBe(false);
    expect(isClearAlias("   ", claude)).toBe(false);
  });

  it("returns false when the alias list is empty (e.g. gemini)", () => {
    expect(isClearAlias("/clear", [])).toBe(false);
    expect(isClearAlias("/new", [])).toBe(false);
  });

  it("does not cross-match aliases between agents", () => {
    expect(isClearAlias("/new", claude)).toBe(false);
    expect(isClearAlias("/clear", codex)).toBe(false);
  });
});

describe("isSubagentToolName", () => {
  it("matches opencode's `task` wire name", () => {
    expect(isSubagentToolName("task", resolveAgentProfile("opencode"))).toBe(true);
  });

  it("does not match a non-subagent opencode tool", () => {
    expect(isSubagentToolName("bash", resolveAgentProfile("opencode"))).toBe(false);
  });

  it("does not match `task` for an agent that doesn't declare it", () => {
    // codex has capabilities.subagents=false and no subagentToolNames.
    expect(isSubagentToolName("task", resolveAgentProfile("codex"))).toBe(false);
    // claude declares subagents but leaves subagentToolNames empty (linkage-based).
    expect(isSubagentToolName("task", resolveAgentProfile("claude"))).toBe(false);
  });

  it("returns false for nullish raw names", () => {
    expect(isSubagentToolName(undefined, resolveAgentProfile("opencode"))).toBe(false);
    expect(isSubagentToolName(null, resolveAgentProfile("opencode"))).toBe(false);
    expect(isSubagentToolName("", resolveAgentProfile("opencode"))).toBe(false);
  });
});
