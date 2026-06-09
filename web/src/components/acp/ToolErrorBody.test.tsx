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
  it("renders children verbatim when status is 'running'", () => {
    const { getByText, queryByText } = render(
      <ToolErrorBody status="running" errorText="ignored">
        <div>child body</div>
      </ToolErrorBody>,
    );
    expect(getByText("child body")).toBeTruthy();
    expect(queryByText(/tool failed/i)).toBeNull();
  });

  it("renders children verbatim when status is 'ok'", () => {
    const { getByText, queryByText } = render(
      <ToolErrorBody status="ok" errorText="ignored">
        <div>child body</div>
      </ToolErrorBody>,
    );
    expect(getByText("child body")).toBeTruthy();
    expect(queryByText(/tool failed/i)).toBeNull();
  });

  it("renders the error chrome and the unwrapped body on status 'err'", () => {
    const { getByText, container } = render(
      <ToolErrorBody status="err" errorText="<tool_use_error>File does not exist.</tool_use_error>">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(getByText(/tool failed/i)).toBeTruthy();
    expect(getByText("agent-reported error")).toBeTruthy();
    expect(container.textContent).toContain("File does not exist.");
  });

  it("hides the attempted-action details by default on err", () => {
    const { container } = render(
      <ToolErrorBody status="err" errorText="<error>boom</error>">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    const details = container.querySelector("details");
    expect(details).not.toBeNull();
    expect(details?.hasAttribute("open")).toBe(false);
  });

  it("renders the summary copy on the collapsible details", () => {
    const { getByText } = render(
      <ToolErrorBody status="err" errorText="<error>boom</error>">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(getByText(/Show attempted action/i)).toBeTruthy();
  });

  it("renders an explicit fallback when errorText is empty / missing", () => {
    const { container } = render(
      <ToolErrorBody status="err" errorText="">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(container.textContent).toContain("(no error detail provided)");
  });

  it("renders the explicit fallback when errorText is undefined", () => {
    const { container } = render(
      <ToolErrorBody status="err">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(container.textContent).toContain("(no error detail provided)");
  });

  it("omits the wrapper-tag chip when the error body is raw (no wrapper)", () => {
    const { queryByText, container } = render(
      <ToolErrorBody status="err" errorText="file not found: foo.rs">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(container.textContent).toContain("file not found: foo.rs");
    // describeToolErrorTag(null) is null → no chip rendered.
    expect(queryByText("agent-reported error")).toBeNull();
  });

  it("passes through arbitrary single-pair wrapper tags as the chip label", () => {
    const { getByText, container } = render(
      <ToolErrorBody status="err" errorText="<custom_wrapper>weird failure</custom_wrapper>">
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    expect(getByText("custom_wrapper")).toBeTruthy();
    expect(container.textContent).toContain("weird failure");
  });

  it("does not render the attempted-action details when children is empty", () => {
    const { container } = render(
      <ToolErrorBody status="err" errorText="boom">
        {null}
      </ToolErrorBody>,
    );
    expect(container.querySelector("details")).toBeNull();
  });

  it("preserves linebreaks in the error body", () => {
    const { container } = render(
      <ToolErrorBody status="err" errorText={"<tool_use_error>line one\nline two\nline three</tool_use_error>"}>
        <div>attempted body</div>
      </ToolErrorBody>,
    );
    const pre = container.querySelector("pre");
    expect(pre).not.toBeNull();
    expect(pre?.textContent).toBe("line one\nline two\nline three");
  });
});
