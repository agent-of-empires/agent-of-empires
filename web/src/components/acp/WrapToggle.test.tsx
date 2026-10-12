// @vitest-environment jsdom
//
// Per-block line wrap: blocks start from `acp.wrap_tool_output`, each block
// toggles on its own, and wrapped output is one element per source line.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

vi.mock("../../lib/snippetHighlighter", () => ({
  highlightSnippet: vi.fn().mockResolvedValue(null),
}));

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));

import { AcpPrefsProvider, type AcpPrefs } from "../../lib/acpPrefs";
import type { AnsiSegment } from "../../lib/ansi";
import { HighlightedBlock, RawBlock, splitAnsiLines } from "./ToolCardChrome";

afterEach(cleanup);

function prefs(wrapToolOutput: boolean): AcpPrefs {
  return {
    showToolDurations: true,
    wrapToolOutput,
    replayEvents: 0,
    compactionReminder: false,
    compactionReminderPercent: 75,
  };
}

function renderWith(wrapToolOutput: boolean, ui: React.ReactNode) {
  return render(<AcpPrefsProvider value={prefs(wrapToolOutput)}>{ui}</AcpPrefsProvider>);
}

describe("RawBlock wrap toggle", () => {
  it("scrolls by default and wraps per line after a tap", () => {
    const { container } = renderWith(false, <RawBlock label="output" text={"first\nsecond"} />);
    const toggle = screen.getByRole("button", { name: /wrap/i });
    expect(toggle.getAttribute("aria-pressed")).toBe("false");
    expect(container.querySelector(".wrap-lines")).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-pressed")).toBe("true");
    const lines = container.querySelectorAll(".wrap-lines .wrap-line");
    expect([...lines].map((l) => l.textContent)).toEqual(["first", "second"]);

    fireEvent.click(toggle);
    expect(container.querySelector(".wrap-lines")).toBeNull();
  });

  it("starts wrapped when the setting is on", () => {
    const { container } = renderWith(true, <RawBlock label="input" text="abc" />);
    expect(screen.getByRole("button", { name: /wrap/i }).getAttribute("aria-pressed")).toBe("true");
    expect(container.querySelector(".wrap-lines")).not.toBeNull();
  });
});

describe("HighlightedBlock wrap toggle", () => {
  it("wraps plain and ANSI output, each toggling independently", () => {
    const { container } = renderWith(
      false,
      <>
        <HighlightedBlock text={"plain one\nplain two"} />
        <HighlightedBlock text={"\u001b[31mred\u001b[0m\nnext"} />
      </>,
    );
    const [plainToggle, ansiToggle] = screen.getAllByRole("button", { name: /wrap/i });
    const [plain, ansi] = [...container.querySelectorAll("pre")];

    fireEvent.click(plainToggle!);
    expect(plain!.classList.contains("wrap-lines")).toBe(true);
    expect(ansi!.classList.contains("wrap-lines")).toBe(false);

    fireEvent.click(ansiToggle!);
    expect([...ansi!.querySelectorAll(".wrap-line")].map((l) => l.textContent)).toEqual(["red", "next"]);
  });
});

describe("splitAnsiLines", () => {
  const seg = (text: string, fg?: string): AnsiSegment => ({ text, style: fg ? { fg } : {} });

  it.each([
    ["no newline", [seg("ab")], [["ab"]]],
    ["newline inside a segment", [seg("a\nb")], [["a"], ["b"]]],
    ["segment spanning lines keeps styles", [seg("a", "red"), seg("b\nc", "blue")], [["a", "b"], ["c"]]],
    ["blank lines survive", [seg("a\n\nb")], [["a"], [], ["b"]]],
    ["trailing newline", [seg("a\n")], [["a"], []]],
  ])("%s", (_name, input, expected) => {
    expect(splitAnsiLines(input).map((line) => line.map((s) => s.text))).toEqual(expected);
  });

  it("preserves style and url per piece", () => {
    const out = splitAnsiLines([{ text: "x\ny", style: { fg: "red" }, url: "https://e.test" }]);
    expect(out).toEqual([
      [{ text: "x", style: { fg: "red" }, url: "https://e.test" }],
      [{ text: "y", style: { fg: "red" }, url: "https://e.test" }],
    ]);
  });
});
