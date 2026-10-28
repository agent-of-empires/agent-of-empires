// @vitest-environment jsdom
//
// Each block of the structured view offers a copy of its own content. The clipboard is
// mocked: what matters here is the text each button hands over.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import type { Plan } from "../../../lib/acpTypes";
import { writeClipboard } from "../../../lib/clipboard";
import { StringDiff } from "../../diff/StringDiff";
import { ElicitationAnswerCard } from "../ElicitationAnswerCard";
import { PlanStrip } from "../PlanStrip";
import { ToolErrorBody } from "../ToolErrorBody";
import { ToolOutputMedia } from "../ToolOutputMedia";
import { HighlightedBlock, RawBlock } from "../ToolCardChrome";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));
vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "vitesse-dark", appearance: "dark" }),
}));
vi.mock("../../../lib/snippetHighlighter", () => ({ highlightSnippet: vi.fn().mockResolvedValue(null) }));
vi.mock("../../../hooks/useHighlightedLines", () => ({ useHighlightedLines: () => ({ tokens: null }) }));

beforeEach(() => {
  vi.mocked(writeClipboard).mockClear();
});

afterEach(cleanup);

describe("copy actions", () => {
  it("tool input and output copy exactly the text shown", () => {
    const { getByRole } = render(
      <>
        <RawBlock label="input" text='{"path":"a.rs"}' />
        <RawBlock label="output" text="fn a() {}" />
      </>,
    );
    fireEvent.click(getByRole("button", { name: "Copy input" }));
    expect(writeClipboard).toHaveBeenLastCalledWith('{"path":"a.rs"}');
    fireEvent.click(getByRole("button", { name: "Copy output" }));
    expect(writeClipboard).toHaveBeenLastCalledWith("fn a() {}");
  });

  it("a highlighted block copies all of its text, not the truncated view", async () => {
    const full = Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n");
    const { getByRole, container } = render(<HighlightedBlock text={full} maxLines={5} />);
    expect(container.textContent).not.toContain("line 29");
    fireEvent.click(getByRole("button", { name: "Copy output" }));
    await waitFor(() => expect(writeClipboard).toHaveBeenCalledWith(full));
  });

  it("a failed tool copies its error detail without the tag chip", () => {
    const { getByRole } = render(
      <ToolErrorBody status="err" errorText="<tool_use_error>File not found</tool_use_error>">
        {null}
      </ToolErrorBody>,
    );
    fireEvent.click(getByRole("button", { name: "Copy error" }));
    expect(writeClipboard).toHaveBeenCalledWith("File not found");
  });

  it("an edit diff copies as a unified diff", () => {
    const { getByRole } = render(<StringDiff oldText={"a\nb\n"} newText={"a\nB\n"} filePath="src/x.rs" />);
    fireEvent.click(getByRole("button", { name: "Copy diff" }));
    const copied = vi.mocked(writeClipboard).mock.lastCall?.[0] ?? "";
    expect(copied.split("\n").slice(0, 2)).toEqual(["--- src/x.rs", "+++ src/x.rs"]);
    expect(copied).toContain("\n-b\n+B");
  });

  it("the plan copies as a checklist once expanded", () => {
    const plan: Plan = {
      plan_id: "p",
      version: 1,
      steps: [
        { id: "1", title: "Read the code", status: "Done" },
        { id: "2", title: "Write the fix", status: "InProgress" },
        { id: "3", title: "Ship it", status: "Pending" },
      ],
    };
    const { getByRole, queryByRole } = render(<PlanStrip plan={plan} />);
    expect(queryByRole("button", { name: "Copy plan" })).toBeNull();
    fireEvent.click(getByRole("button", { name: /Write the fix/ }));
    fireEvent.click(getByRole("button", { name: "Copy plan" }));
    expect(writeClipboard).toHaveBeenCalledWith(
      "- [x] Read the code\n- [ ] Write the fix (in progress)\n- [ ] Ship it",
    );
  });

  it("answers copy as bold questions followed by the answer", () => {
    const { getByRole } = render(
      <ElicitationAnswerCard
        answers={[
          { question: "Which env?", answer: "staging" },
          { question: "Dry run?", answer: "yes" },
        ]}
      />,
    );
    fireEvent.click(getByRole("button", { name: "Copy answers" }));
    expect(writeClipboard).toHaveBeenCalledWith("**Which env?**\nstaging\n\n**Dry run?**\nyes");
  });

  it("embedded resource text copies from beside its block, while an image is left to the browser", () => {
    const { getByRole, getAllByRole } = render(
      <ToolOutputMedia
        blocks={[
          { kind: "image", mime_type: "image/png", data: "AAAA" },
          { kind: "resource", uri: "file:///notes.md", mime_type: "text/markdown", text: "# Notes\n- one" },
        ]}
      />,
    );
    fireEvent.click(getByRole("button", { name: "Copy text" }));
    expect(writeClipboard).toHaveBeenCalledWith("# Notes\n- one");
    // The image has its own expand button, but no copy button.
    expect(getAllByRole("button", { name: /copy/i })).toHaveLength(1);
  });
});
