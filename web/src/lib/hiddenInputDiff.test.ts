import { describe, expect, it } from "vitest";
import { HIDDEN_INPUT_SENTINEL as S, stepHiddenInput } from "./hiddenInputDiff";

/** Feeds successive textarea values (after the sentinel) and returns the wire bytes and the final value. */
function replay(values: string[]) {
  let baseline = S;
  let wire = "";
  for (const value of values) {
    const step = stepHiddenInput(baseline, value);
    wire += "\x7f".repeat(step.deleted) + step.inserted;
    baseline = step.refill + value;
  }
  return { wire, value: baseline };
}

describe("stepHiddenInput", () => {
  it.each<[string, string[], string, string]>([
    ["typing sends each insertion", [S + "l", S + "ls"], "ls", S + "ls"],
    // iOS Korean (#3692): each keystroke rewrites the trailing syllable in place.
    ["a Korean syllable rewrite", [S + "ㅎ", S, S + "하", S, S + "한"], "ㅎ\x7f하\x7f한", S + "한"],
    ["a Korean rewrite as one replacement", [S + "ㅎ", S + "하", S + "한"], "ㅎ\x7f하\x7f한", S + "한"],
    [
      // iOS dictation replaces its hypothesis in place, then corrects the whole phrase at the end.
      "dictation hypotheses and a final correction",
      [S + "thi", S + "this", S + "this is", S + "this is a", S + "this is a test", S + "This is a test."],
      "thi" + "s" + " is" + " a" + " test" + "\x7f".repeat(14) + "This is a test.",
      S + "This is a test.",
    ],
    [
      "held backspace deletes into the sentinel and refills it",
      [S + "ab", S + "a", S, S.slice(1), S.slice(1), S.slice(1)],
      "ab" + "\x7f".repeat(5),
      S,
    ],
    [
      "a word delete sends one DEL per code point",
      [S + "git status", S + "git "],
      "git status\x7f\x7f\x7f\x7f\x7f\x7f",
      S + "git ",
    ],
    ["a word delete past the typed text", [S + "ls", S.slice(1)], "ls\x7f\x7f\x7f", S],
    ["an emoji is deleted as one code point", [S + "hi\u{1F642}", S + "hi"], "hi\u{1F642}\x7f", S + "hi"],
    // Two emoji sharing a high surrogate must not diff inside the pair.
    [
      "an emoji replaced by its neighbour",
      [S + "\u{1F642}", S + "\u{1F643}"],
      "\u{1F642}\x7f\u{1F643}",
      S + "\u{1F643}",
    ],
    // SwiftKey (#3746) re-wraps the typed word in a composition that commits the same or a longer word.
    ["a composition commit that adopts the typed word", [S + "tes", S + "test", S + "test"], "test", S + "test"],
    ["a composition commit that stands on its own", [S + "a", S + "a日本"], "a日本", S + "a日本"],
    ["an edit that replaced the whole value", ["x"], "\x7f\x7f\x7fx", S + "x"],
  ])("%s", (_name, values, wire, value) => {
    expect(replay(values)).toEqual({ wire, value });
  });
});
