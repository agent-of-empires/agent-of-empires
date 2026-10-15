// @vitest-environment jsdom
//
// Shared wrapper around per-kind tool-card bodies that surfaces the
// adapter's failure reason on err status. Without it, EditToolCard
// and similar drop the error text on the floor and the only signal
// is a tiny status dot.
//
// The parsing helper (parseToolError / describeToolErrorTag) is
// covered by toolErrorParse.test.ts; this spec pins what the wrapper
// does with the parser output.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { ToolErrorBody } from "./ToolErrorBody";

afterEach(() => {
  cleanup();
});

describe("ToolErrorBody", () => {
  it("renders children verbatim unless status is 'err'", () => {
    for (const status of ["running", "ok"] as const) {
      const { getByText, queryByText } = render(
        <ToolErrorBody status={status} errorText="ignored">
          <div>child body</div>
        </ToolErrorBody>,
      );
      expect(getByText("child body")).toBeTruthy();
      expect(queryByText(/tool failed/i)).toBeNull();
      cleanup();
    }
  });

  it("on err, shows the unwrapped body with linebreaks and the attempted action collapsed", () => {
    const { getByText, container } = render(
      <ToolErrorBody status="err" errorText={"<tool_use_error>line one\nline two</tool_use_error>"}>
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(getByText(/tool failed/i)).toBeTruthy();
    expect(getByText("agent-reported error")).toBeTruthy();
    expect(container.querySelector("pre")?.textContent).toBe("line one\nline two");
    expect(getByText(/Show attempted action/i)).toBeTruthy();
    expect(container.querySelector("details")?.hasAttribute("open")).toBe(false);
  });

  it("labels the chip from the wrapper tag and omits it for a raw body", () => {
    const cases: [string, string, string | null][] = [
      ["<custom_wrapper>weird failure</custom_wrapper>", "weird failure", "custom_wrapper"],
      ["file not found: foo.rs", "file not found: foo.rs", null],
    ];
    for (const [errorText, body, chip] of cases) {
      const { queryByText, container } = render(
        <ToolErrorBody status="err" errorText={errorText}>
          <div>attempted body</div>
        </ToolErrorBody>,
      );
      expect(container.textContent).toContain(body);
      if (chip) expect(queryByText(chip)).toBeTruthy();
      expect(queryByText("agent-reported error")).toBeNull();
      cleanup();
    }
  });

  it("renders an explicit fallback when errorText is empty or missing", () => {
    for (const errorText of ["", undefined]) {
      const { container } = render(
        <ToolErrorBody status="err" errorText={errorText}>
          <div>attempted body</div>
        </ToolErrorBody>,
      );
      expect(container.textContent).toContain("(no error detail provided)");
      cleanup();
    }
  });

  it("does not render the attempted-action details when children is empty", () => {
    const { container } = render(
      <ToolErrorBody status="err" errorText="boom">
        {null}
      </ToolErrorBody>,
    );
    expect(container.querySelector("details")).toBeNull();
  });
});
