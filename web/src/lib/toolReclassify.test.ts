import { describe, expect, it } from "vitest";
import { reclassifyBash } from "./toolReclassify";
import type { ToolCall } from "./acpTypes";

function bash(command: string, kind = "execute"): ToolCall {
  return {
    id: "tc-1",
    name: "Bash",
    kind,
    args_preview: JSON.stringify({ command }),
    started_at: "2026-01-01T00:00:00Z",
  };
}

describe("reclassifyBash", () => {
  it("reclassifies plain grep/rg/find/fd shellouts as search", () => {
    expect(reclassifyBash(bash("grep -rn foo .")).kind).toBe("search");
    expect(reclassifyBash(bash("rg --hidden pattern src/")).kind).toBe(
      "search",
    );
    expect(reclassifyBash(bash("find . -name '*.tsx'")).kind).toBe("search");
    expect(reclassifyBash(bash("fd '\\.rs$' src")).kind).toBe("search");
    expect(reclassifyBash(bash("ripgrep pattern src/")).kind).toBe("search");
  });

  it("tags reclassified calls with the bash provenance", () => {
    expect(reclassifyBash(bash("grep foo")).provenance).toBe("bash");
  });

  it("leaves the command alone when it isn't a known search binary", () => {
    expect(reclassifyBash(bash("npm install")).kind).toBe("execute");
    expect(reclassifyBash(bash("cargo build")).kind).toBe("execute");
    expect(reclassifyBash(bash("./run.sh")).kind).toBe("execute");
  });

  it("rejects pipes and command chaining as not-search", () => {
    expect(reclassifyBash(bash("grep foo | wc -l")).kind).toBe("execute");
    expect(reclassifyBash(bash("grep foo; echo done")).kind).toBe("execute");
    expect(reclassifyBash(bash("grep foo && rm bar")).kind).toBe("execute");
  });

  it("rejects redirects that turn a search into a write", () => {
    expect(reclassifyBash(bash("grep foo > out.txt")).kind).toBe("execute");
    expect(reclassifyBash(bash("grep foo >> log")).kind).toBe("execute");
  });

  it("rejects destructive find flags", () => {
    expect(reclassifyBash(bash("find . -name '*.tmp' -delete")).kind).toBe(
      "execute",
    );
    expect(reclassifyBash(bash("find . -exec rm {} +")).kind).toBe("execute");
  });

  it("passes through non-execute kinds unchanged", () => {
    const t: ToolCall = {
      id: "t",
      name: "Read",
      kind: "read",
      args_preview: "{}",
      started_at: "2026-01-01T00:00:00Z",
    };
    expect(reclassifyBash(t).kind).toBe("read");
    expect(reclassifyBash(t).provenance).toBeNull();
  });

  it("is a no-op when the args don't carry a command string", () => {
    const t: ToolCall = {
      id: "t",
      name: "Bash",
      kind: "execute",
      args_preview: "{}",
      started_at: "2026-01-01T00:00:00Z",
    };
    expect(reclassifyBash(t).kind).toBe("execute");
  });
});
