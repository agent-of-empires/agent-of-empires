// @vitest-environment jsdom
//
// Rendering semantics for #1472. The structured view Markdown wrapper delegates
// the markdown -> HTML transform to @assistant-ui/react-markdown's
// MarkdownTextPrimitive, which needs a deep assistant-ui runtime context
// to mount and so cannot render standalone in jsdom (see Markdown.test.tsx,
// which mocks the primitive and asserts the wrapper config instead).
//
// To exercise the actual line-break behaviour deterministically we render
// the SAME remark plugin chain the component mounts (`remarkPluginsFor`)
// through react-markdown, the engine the primitive wraps. The
// breaks=false vs breaks=true contrast is the regression: with the old
// chain a single newline collapses to whitespace; with remark-breaks it
// becomes a <br>.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import ReactMarkdown from "react-markdown";

import { remarkPluginsFor } from "./Markdown";

function renderMd(text: string, breaks: boolean) {
  return render(
    <ReactMarkdown remarkPlugins={remarkPluginsFor(breaks)}>
      {text}
    </ReactMarkdown>,
  );
}

describe("remarkPluginsFor rendering (#1472)", () => {
  it("collapses single newlines to whitespace without breaks (assistant default)", () => {
    const { container } = renderMd("line a\nline b\nline c", false);
    expect(container.querySelectorAll("br")).toHaveLength(0);
    expect(container.querySelectorAll("p")).toHaveLength(1);
  });

  it("renders single newlines as <br> when breaks is enabled (user prompt)", () => {
    const { container } = renderMd("line a\nline b\nline c", true);
    expect(container.querySelectorAll("br")).toHaveLength(2);
    expect(container.textContent).toContain("line a");
    expect(container.textContent).toContain("line b");
    expect(container.textContent).toContain("line c");
  });

  it("renders a blank line as a paragraph break when breaks is enabled", () => {
    const { container } = renderMd("para one\n\npara two", true);
    const paragraphs = container.querySelectorAll("p");
    expect(paragraphs).toHaveLength(2);
    expect(paragraphs[0]?.textContent).toContain("para one");
    expect(paragraphs[1]?.textContent).toContain("para two");
    expect(container.querySelectorAll("br")).toHaveLength(0);
  });

  it("leaves fenced code blocks intact when breaks is enabled", () => {
    const { container } = renderMd(
      "```ts\nconst a = 1;\nconst b = 2;\n```",
      true,
    );
    const pre = container.querySelector("pre");
    expect(pre).not.toBeNull();
    expect(pre?.textContent).toContain("const a = 1;");
    expect(pre?.textContent).toContain("const b = 2;");
    // The newlines inside the code block must not become <br> nodes.
    expect(container.querySelectorAll("pre br")).toHaveLength(0);
    // And the fences themselves are absorbed, not rendered as literal text.
    expect(container.textContent).not.toContain("```");
  });
});
