// Range math for the composer's `/` picker (#3418). The bug these lock
// down: the old insert appended `/<cmd>` to the end of the buffer and
// never looked at the caret, so a command picked mid-message landed in
// the wrong place and the typed token was left behind.

import { describe, expect, it } from "vitest";

import { findSlashTokenRange, replaceSlashCommand } from "./slashCompletion";

describe("findSlashTokenRange", () => {
  it("finds the token the caret sits in, and only at a valid boundary", () => {
    const cases: [string, number, { start: number; end: number } | null][] = [
      ["/he", 3, { start: 0, end: 3 }],
      ["fix /he the bug", 7, { start: 4, end: 7 }],
      // Caret mid-token: the range still covers the whole token, so the
      // tail does not survive the completion.
      ["/addrXYZ", 5, { start: 0, end: 8 }],
      // `\n` is whitespace to assistant-ui's detector too, so a
      // multi-line composer resolves per line.
      ["a\n/he", 5, { start: 2, end: 5 }],
      ["/he\nnext", 3, { start: 0, end: 3 }],
      // Not preceded by whitespace, so not a trigger.
      ["foo/bar", 7, null],
      ["https://example", 15, null],
      // Nothing to the left of the caret but whitespace.
      ["hello ", 6, null],
      ["", 0, null],
    ];
    for (const [value, caret, expected] of cases) {
      expect(findSlashTokenRange(value, caret), `${JSON.stringify(value)}@${caret}`).toEqual(expected);
    }
  });
});

describe("replaceSlashCommand", () => {
  it("replaces the caret's token and parks the caret past the boundary", () => {
    // [value, selectionStart, selectionEnd, expected text, expected cursor]
    const cases: [string, number, number, string, number][] = [
      ["", 0, 0, "/help ", 6],
      ["/h", 2, 2, "/help ", 6],
      // The mid-buffer case from the report: the command lands where the
      // user was typing, not at the end.
      ["fix /he the bug", 7, 7, "fix /help the bug", 10],
      ["fix /he", 7, 7, "fix /help ", 10],
      ["/addrXYZ", 5, 5, "/help ", 6],
      ["a\n/he", 5, 5, "a\n/help ", 8],
      // An existing whitespace boundary is reused rather than doubled,
      // and a newline stays a newline.
      ["/he\nnext", 3, 3, "/help\nnext", 6],
      ["/he next", 3, 3, "/help next", 6],
    ];
    for (const [value, start, end, text, cursor] of cases) {
      expect(replaceSlashCommand(value, start, end, "help"), JSON.stringify(value)).toEqual({ text, cursor });
    }
  });

  it("inserts at the caret when there is no token, padding a mid-word caret", () => {
    const cases: [string, number, number, string, number][] = [
      ["hello", 5, 5, "hello /help ", 12],
      ["hello ", 6, 6, "hello /help ", 12],
      ["ab cd", 2, 2, "ab /help cd", 9],
      // A `/` that is not a trigger must not be swallowed.
      ["https://example", 15, 15, "https://example /help ", 22],
      ["foo/bar", 7, 7, "foo/bar /help ", 14],
      // A selection is replaced by the command.
      ["pick me", 5, 7, "pick /help ", 11],
    ];
    for (const [value, start, end, text, cursor] of cases) {
      expect(replaceSlashCommand(value, start, end, "help"), JSON.stringify(value)).toEqual({ text, cursor });
    }
  });
});
