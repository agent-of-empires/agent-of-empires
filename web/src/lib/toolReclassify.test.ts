import { expect, it } from "vitest";
import { reclassifyBash } from "./toolReclassify";
import type { ToolCall } from "./acpTypes";

function call(command: string | null, kind = "execute"): ToolCall {
  const args = command === null ? {} : { command };
  return { id: "tc-1", name: "Bash", kind, args_preview: JSON.stringify(args), started_at: "2026-01-01T00:00:00Z" };
}

it("reclassifies only plain search shellouts as search, tagged with bash provenance", () => {
  for (const cmd of ["grep -rn foo .", "rg --hidden pattern src/", "find . -name '*.tsx'", "fd '\\.rs$' src"]) {
    expect(reclassifyBash(call(cmd)), cmd).toMatchObject({ kind: "search", provenance: "bash" });
  }
  for (const cmd of [
    "npm install",
    "./run.sh",
    "grep foo | wc -l",
    "grep foo; echo done",
    "grep foo && rm bar",
    "grep foo > out.txt",
    "grep foo >> log",
    "find . -name '*.tmp' -delete",
    "find . -exec rm {} +",
  ]) {
    expect(reclassifyBash(call(cmd)).kind, cmd).toBe("execute");
  }
});

it("passes through non-execute kinds and calls without a command", () => {
  expect(reclassifyBash(call("grep foo", "read"))).toMatchObject({ kind: "read", provenance: null });
  expect(reclassifyBash(call(null)).kind).toBe("execute");
});
