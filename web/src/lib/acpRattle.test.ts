import { expect, it } from "vitest";

import { chooseVerb, deriveSpinnerState, pickIndex, THINKING_VERBS, TOOL_LABEL_MAX, WORKING_VERBS } from "./acpRattle";

it("pickIndex is deterministic, in range, and safe for an empty pool", () => {
  for (let s = -10; s < 200; s++) {
    const r = pickIndex(7, s);
    expect(r).toBe(pickIndex(7, s));
    expect(r).toBeGreaterThanOrEqual(0);
    expect(r).toBeLessThan(7);
  }
  expect(pickIndex(0, 42)).toBe(0);
  expect(pickIndex(0, 0)).toBe(0);
});

it("chooseVerb picks a stable verb from the pool for its state", () => {
  const cases: [Parameters<typeof chooseVerb>, readonly string[]][] = [
    [["working", 7], WORKING_VERBS],
    [["thinking", 7], THINKING_VERBS],
    [["tool", 1, null], WORKING_VERBS],
    [["tool", 1, ""], WORKING_VERBS],
  ];
  for (const [args, pool] of cases) {
    const out = chooseVerb(...args);
    expect(out, String(args)).toBe(chooseVerb(...args));
    expect(out.endsWith("…"), String(args)).toBe(true);
    expect(pool, String(args)).toContain(out.slice(0, -1));
  }
  expect(chooseVerb("tool", 3, "Read foo.ts").endsWith(" Read foo.ts…")).toBe(true);
});

it("chooseVerb clamps a long tool title so it does not flood the spinner (#1728)", () => {
  const longCommand =
    "cat <<'__CONSULT_LLM_END__' | consult-llm --task plan -m gemini -m openai -f /Users/foo/terminal_handler.rs";
  const out = chooseVerb("tool", 3, longCommand);
  expect(out.endsWith("…")).toBe(true);
  expect(out).not.toContain("consult-llm");
  const verb = out.split(" ")[0];
  const inlined = out.slice(verb.length + 1, -1);
  expect(inlined.length).toBeLessThanOrEqual(TOOL_LABEL_MAX);
  expect(longCommand.startsWith(inlined.trimEnd())).toBe(true);
});

it("deriveSpinnerState prefers tool, then thinking, then working (#1213)", () => {
  expect(deriveSpinnerState(true, "Terminal")).toBe("tool");
  expect(deriveSpinnerState(false, "Terminal")).toBe("tool");
  expect(deriveSpinnerState(true, null)).toBe("thinking");
  expect(deriveSpinnerState(false, null)).toBe("working");
});
