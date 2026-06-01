// Spinner verb selection. The verb cycle is purely cosmetic, but
// pickIndex must be deterministic per seed (otherwise the verb flips
// every render and reads as a glitch) and stay in-bounds for any
// pool size including zero.

import { describe, expect, it } from "vitest";

import {
  chooseVerb,
  deriveSpinnerState,
  pickIndex,
  SPINNER_FRAMES,
  SPINNER_INTERVAL_MS,
  THINKING_VERBS,
  TOOL_LABEL_MAX,
  VERB_INTERVAL_MS,
  WORKING_VERBS,
} from "./cockpitRattle";

describe("pickIndex", () => {
  it("is deterministic for a fixed seed", () => {
    const seeds = [0, 1, 17, 42, 99, 1_000_000];
    for (const s of seeds) {
      const a = pickIndex(10, s);
      const b = pickIndex(10, s);
      expect(a).toBe(b);
    }
  });

  it("stays in range [0, len)", () => {
    for (let s = 0; s < 200; s++) {
      const r = pickIndex(7, s);
      expect(r).toBeGreaterThanOrEqual(0);
      expect(r).toBeLessThan(7);
    }
  });

  it("returns 0 for an empty pool without dividing by zero", () => {
    expect(pickIndex(0, 42)).toBe(0);
    expect(Number.isNaN(pickIndex(0, 0))).toBe(false);
  });

  it("handles negative-ish hash output (Math.abs guard)", () => {
    // The mulberry-ish hash can produce sign-flipped intermediate
    // values; any seed must yield a non-negative index.
    for (let s = -10; s <= 10; s++) {
      const r = pickIndex(5, s);
      expect(r).toBeGreaterThanOrEqual(0);
    }
  });
});

describe("chooseVerb", () => {
  it("appends … and picks from WORKING_VERBS in the working state", () => {
    const out = chooseVerb("working", 7);
    expect(out.endsWith("…")).toBe(true);
    const verb = out.slice(0, -1);
    expect(WORKING_VERBS).toContain(verb);
  });

  it("picks from THINKING_VERBS in the thinking state", () => {
    const out = chooseVerb("thinking", 7);
    expect(out.endsWith("…")).toBe(true);
    const verb = out.slice(0, -1);
    expect(THINKING_VERBS).toContain(verb);
  });

  it("formats tool state as '<verb> <toolName>…' when toolName is set", () => {
    const out = chooseVerb("tool", 3, "Bash");
    expect(out.endsWith("…")).toBe(true);
    expect(out.endsWith(" Bash…")).toBe(true);
  });

  it("clamps a long tool title so it does not flood the spinner (#1728)", () => {
    // inFlightTool.name carries the ACP title, which for Bash is the full
    // command line. A heredoc-into-consult-llm dispatch can run hundreds
    // of chars; the inline spinner must not inline it verbatim.
    const longCommand =
      "cat <<'__CONSULT_LLM_END__' | consult-llm --task plan -m gemini -m openai -f /Users/foo/terminal_handler.rs";
    const out = chooseVerb("tool", 3, longCommand);
    expect(out.endsWith("…")).toBe(true);
    // The inlined name is bounded; the full command never reaches the row.
    expect(out).not.toContain(longCommand);
    expect(out).not.toContain("consult-llm");
    // Whole label = "<verb> <clamped>…"; clamped slice is <= TOOL_LABEL_MAX.
    const verb = out.split(" ")[0];
    const inlined = out.slice(verb.length + 1, -1);
    expect(inlined.length).toBeLessThanOrEqual(TOOL_LABEL_MAX);
    expect(longCommand.startsWith(inlined.trimEnd())).toBe(true);
  });

  it("leaves short tool titles unchanged (#1728)", () => {
    expect(chooseVerb("tool", 3, "Bash").endsWith(" Bash…")).toBe(true);
    expect(chooseVerb("tool", 3, "Read foo.ts").endsWith(" Read foo.ts…")).toBe(
      true,
    );
  });

  it("falls back to WORKING_VERBS when state=tool but toolName is null", () => {
    const out = chooseVerb("tool", 1, null);
    expect(out.endsWith("…")).toBe(true);
    expect(WORKING_VERBS).toContain(out.slice(0, -1));
  });

  it("falls back to WORKING_VERBS when state=tool but toolName is empty", () => {
    const out = chooseVerb("tool", 1, "");
    expect(out.endsWith("…")).toBe(true);
    expect(WORKING_VERBS).toContain(out.slice(0, -1));
  });

  it("returns the same label for the same (state, seed, toolName)", () => {
    expect(chooseVerb("working", 42)).toBe(chooseVerb("working", 42));
    expect(chooseVerb("thinking", 42)).toBe(chooseVerb("thinking", 42));
    expect(chooseVerb("tool", 42, "Read")).toBe(
      chooseVerb("tool", 42, "Read"),
    );
  });
});

describe("deriveSpinnerState (#1213)", () => {
  it("prefers tool over thinking when both are set", () => {
    expect(deriveSpinnerState(true, "Terminal")).toBe("tool");
  });

  it("returns tool when only a tool is in flight", () => {
    expect(deriveSpinnerState(false, "Terminal")).toBe("tool");
  });

  it("returns thinking when thinking and no tool", () => {
    expect(deriveSpinnerState(true, null)).toBe("thinking");
  });

  it("returns working when neither thinking nor tool", () => {
    expect(deriveSpinnerState(false, null)).toBe("working");
  });
});

describe("constants", () => {
  it("SPINNER_FRAMES is 10 single-codepoint frames", () => {
    expect(SPINNER_FRAMES).toHaveLength(10);
    for (const frame of SPINNER_FRAMES) {
      // Each braille glyph is a single codepoint above U+FFFF? No,
      // they sit in the BMP at U+28xx, so .length === 1 is correct.
      expect(frame).toHaveLength(1);
    }
  });

  it("SPINNER_INTERVAL_MS is a positive number", () => {
    expect(SPINNER_INTERVAL_MS).toBeGreaterThan(0);
  });

  it("VERB_INTERVAL_MS is well above SPINNER_INTERVAL_MS", () => {
    expect(VERB_INTERVAL_MS).toBeGreaterThan(SPINNER_INTERVAL_MS * 10);
  });
});
