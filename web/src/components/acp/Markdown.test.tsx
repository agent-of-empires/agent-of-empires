// @vitest-environment jsdom
//
// Markdown wrapper contract. Scope: the customisations our wrapper
// adds on top of @assistant-ui/react-markdown's MarkdownTextPrimitive.
// The primitive itself needs a deep assistant-ui MessagePart context
// to render, so we mock it and verify the wrapper:
//   - mounts the primitive with `remark-gfm` in remarkPlugins,
//   - forwards the `smooth` prop and the `text` content,
//   - passes a `components` map with our custom Blockquote (warning
//     variant when text starts with the ⚠️ glyph), TableWithScroll,
//     ShikiSyntaxHighlighter, and CodeHeader entries.
//
// Then we exercise the captured custom components directly against
// jsdom to assert their per-component contracts (warning class,
// table-wrap container, copy-button click).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "vitesse-dark", appearance: "dark" }),
}));

vi.mock("../../lib/highlighter", () => ({
  ensureThemeLoaded: vi.fn().mockResolvedValue("vitesse-dark"),
  getHighlighter: vi.fn().mockResolvedValue({
    codeToHtml: () => "<pre><code>highlighted</code></pre>",
  }),
  langKeyForExt: (s: string) => s,
  loadLanguage: vi.fn().mockResolvedValue(undefined),
}));

interface PrimitiveCall {
  text: string;
  smooth: boolean;
  remarkPlugins: unknown[];
  components: Record<string, React.ComponentType<unknown>>;
}

const primitiveCalls: PrimitiveCall[] = [];

vi.mock("@assistant-ui/react-markdown", () => ({
  MarkdownTextPrimitive: (props: {
    preprocess: () => string;
    smooth?: boolean;
    remarkPlugins?: unknown[];
    className?: string;
    components?: Record<string, React.ComponentType<unknown>>;
  }) => {
    primitiveCalls.push({
      text: props.preprocess(),
      smooth: !!props.smooth,
      remarkPlugins: props.remarkPlugins ?? [],
      components: props.components ?? {},
    });
    return <div data-testid="markdown-primitive" className={props.className} />;
  },
}));

import { Markdown } from "./Markdown";
import { AcpFileRefContext } from "./AcpFileRefContext";

beforeEach(() => {
  primitiveCalls.length = 0;
});

afterEach(() => {
  cleanup();
});

describe("Markdown wrapper config", () => {
  it("renders the assistant-ui markdown primitive", () => {
    const { getByTestId } = render(<Markdown text="hi" />);
    expect(getByTestId("markdown-primitive")).toBeTruthy();
  });

  it("forwards the source text via preprocess", () => {
    render(<Markdown text="hello world" />);
    expect(primitiveCalls).toHaveLength(1);
    expect(primitiveCalls[0]!.text).toBe("hello world");
  });

  it("defaults smooth=false and forwards smooth=true on demand", () => {
    render(<Markdown text="a" />);
    render(<Markdown text="b" smooth />);
    expect(primitiveCalls[0]!.smooth).toBe(false);
    expect(primitiveCalls[1]!.smooth).toBe(true);
  });

  it("registers remark-gfm in the plugin list", () => {
    render(<Markdown text="x" />);
    expect(primitiveCalls[0]!.remarkPlugins).toContain(remarkGfm);
  });

  // #1472: user prompts opt into hard line breaks so the sent bubble
  // matches the plain-textarea composer; assistant text leaves it off.
  it("adds remark-breaks only when breaks is enabled", () => {
    render(<Markdown text="x" breaks />);
    render(<Markdown text="y" />);
    expect(primitiveCalls[0]!.remarkPlugins).toContain(remarkBreaks);
    expect(primitiveCalls[0]!.remarkPlugins).toContain(remarkGfm);
    expect(primitiveCalls[1]!.remarkPlugins).not.toContain(remarkBreaks);
  });

  it("registers acp-specific component overrides", () => {
    render(<Markdown text="x" />);
    const keys = Object.keys(primitiveCalls[0]!.components);
    expect(keys).toEqual(expect.arrayContaining(["SyntaxHighlighter", "CodeHeader", "table", "blockquote", "a"]));
  });

  it("attaches the acp-markdown class for global styling", () => {
    const { container } = render(<Markdown text="x" />);
    const node = container.querySelector(".acp-markdown");
    expect(node).not.toBeNull();
  });
});

describe("Blockquote override", () => {
  function getBlockquote(): React.ComponentType<{
    children: React.ReactNode;
  }> {
    render(<Markdown text="x" />);
    const Comp = primitiveCalls.at(-1)!.components.blockquote;
    return Comp as React.ComponentType<{ children: React.ReactNode }>;
  }

  it("applies the warning variant when the text starts with the warning glyph", () => {
    const Blockquote = getBlockquote();
    const { container } = render(<Blockquote>⚠️ context reset</Blockquote>);
    const bq = container.querySelector("blockquote");
    expect(bq).not.toBeNull();
    expect(bq?.className).toContain("acp-callout-warn");
  });

  it("uses no warning class for plain text", () => {
    const Blockquote = getBlockquote();
    const { container } = render(<Blockquote>just a quote</Blockquote>);
    const bq = container.querySelector("blockquote");
    expect(bq).not.toBeNull();
    expect(bq?.className ?? "").not.toContain("acp-callout-warn");
  });

  it("strips leading whitespace before checking for the warning glyph", () => {
    const Blockquote = getBlockquote();
    const { container } = render(<Blockquote> ⚠️ warning</Blockquote>);
    const bq = container.querySelector("blockquote");
    expect(bq?.className).toContain("acp-callout-warn");
  });

  it("walks nested React children when inspecting the text", () => {
    const Blockquote = getBlockquote();
    const { container } = render(
      <Blockquote>
        <span>
          <strong>⚠️</strong> nested warning
        </span>
      </Blockquote>,
    );
    expect(container.querySelector("blockquote")?.className).toContain("acp-callout-warn");
  });
});

// #1714: transcript links must open in a new tab with a safe rel so
// clicking a docs/CI/repo link does not replace the live structured view page.
describe("anchor override", () => {
  function getAnchor(): React.ComponentType<React.ComponentPropsWithoutRef<"a">> {
    render(<Markdown text="x" />);
    return primitiveCalls.at(-1)!.components.a as React.ComponentType<React.ComponentPropsWithoutRef<"a">>;
  }

  it("forces transcript links to open in a new tab with a safe rel", () => {
    const Anchor = getAnchor();
    const { container } = render(<Anchor href="https://example.com">docs</Anchor>);
    const a = container.querySelector("a");
    expect(a).not.toBeNull();
    expect(a?.getAttribute("target")).toBe("_blank");
    expect(a?.getAttribute("rel")).toBe("noopener noreferrer");
  });

  it("preserves the original href and children", () => {
    const Anchor = getAnchor();
    const { container } = render(
      <Anchor href="https://example.com/path" title="t">
        link text
      </Anchor>,
    );
    const a = container.querySelector("a");
    expect(a?.getAttribute("href")).toBe("https://example.com/path");
    expect(a?.getAttribute("title")).toBe("t");
    expect(a?.textContent).toBe("link text");
  });
});

// #1718: a local file reference (Codex `path:line` markdown link) is
// intercepted and routed to the in-app file viewer instead of opening a
// dead new tab. External links keep the #1714 new-tab behavior.
describe("anchor file-ref interception", () => {
  function getAnchor(): React.ComponentType<React.ComponentPropsWithoutRef<"a">> {
    render(<Markdown text="x" />);
    return primitiveCalls.at(-1)!.components.a as React.ComponentType<React.ComponentPropsWithoutRef<"a">>;
  }

  it("intercepts a local file link and calls the file-ref handler", () => {
    const Anchor = getAnchor();
    const onOpenFileRef = vi.fn();
    const { container } = render(
      <AcpFileRefContext.Provider value={{ onOpenFileRef }}>
        <Anchor href="/Users/me/repo/src/app.ts:42">app.ts</Anchor>
      </AcpFileRefContext.Provider>,
    );
    const a = container.querySelector("a")!;
    const event = new MouseEvent("click", { bubbles: true, cancelable: true });
    fireEvent(a, event);
    expect(onOpenFileRef).toHaveBeenCalledWith({
      path: "/Users/me/repo/src/app.ts",
      line: 42,
    });
    expect(event.defaultPrevented).toBe(true);
  });

  it("does not intercept an external link even with a handler present", () => {
    const Anchor = getAnchor();
    const onOpenFileRef = vi.fn();
    const { container } = render(
      <AcpFileRefContext.Provider value={{ onOpenFileRef }}>
        <Anchor href="https://example.com">docs</Anchor>
      </AcpFileRefContext.Provider>,
    );
    const a = container.querySelector("a")!;
    const event = new MouseEvent("click", { bubbles: true, cancelable: true });
    fireEvent(a, event);
    expect(onOpenFileRef).not.toHaveBeenCalled();
    expect(event.defaultPrevented).toBe(false);
    expect(a.getAttribute("target")).toBe("_blank");
    expect(a.getAttribute("rel")).toBe("noopener noreferrer");
  });

  it("leaves links untouched when no handler is provided", () => {
    const Anchor = getAnchor();
    const { container } = render(<Anchor href="/Users/me/repo/src/app.ts:42">app.ts</Anchor>);
    const a = container.querySelector("a")!;
    const event = new MouseEvent("click", { bubbles: true, cancelable: true });
    fireEvent(a, event);
    // No handler in context, so the file link falls through to the
    // default new-tab anchor without preventing navigation.
    expect(event.defaultPrevented).toBe(false);
    expect(a.getAttribute("target")).toBe("_blank");
  });
});

describe("TableWithScroll override", () => {
  function getTable(): React.ComponentType<{
    children: React.ReactNode;
  }> {
    render(<Markdown text="x" />);
    const Comp = primitiveCalls.at(-1)!.components.table;
    return Comp as React.ComponentType<{ children: React.ReactNode }>;
  }

  it("wraps the rendered table in a scroll container", () => {
    const Table = getTable();
    const { container } = render(
      <Table>
        <tbody>
          <tr>
            <td>cell</td>
          </tr>
        </tbody>
      </Table>,
    );
    const wrap = container.querySelector(".acp-table-wrap");
    expect(wrap).not.toBeNull();
    expect(wrap?.querySelector("table")).not.toBeNull();
    expect(wrap?.querySelector("td")?.textContent).toBe("cell");
  });
});

describe("CodeHeader override", () => {
  function getCodeHeader(): React.ComponentType<{
    language?: string;
    code: string;
  }> {
    render(<Markdown text="x" />);
    return primitiveCalls.at(-1)!.components.CodeHeader as React.ComponentType<{
      language?: string;
      code: string;
    }>;
  }

  it("renders the language label", () => {
    const Header = getCodeHeader();
    const { container } = render(<Header language="rust" code="fn main(){}" />);
    expect(container.textContent).toContain("rust");
  });

  it("falls back to 'text' when no language is provided", () => {
    const Header = getCodeHeader();
    const { container } = render(<Header code="abc" />);
    expect(container.textContent).toContain("text");
  });

  it("copies the raw source to the clipboard when the copy button is clicked", () => {
    const Header = getCodeHeader();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    const { getByText } = render(<Header language="js" code="alert('hi')" />);
    fireEvent.click(getByText("copy"));
    expect(writeText).toHaveBeenCalledWith("alert('hi')");
  });
});
