// @vitest-environment jsdom
// The assistant-ui primitive needs a message context, so it is mocked; the wrapper's
// config and each captured override component are tested directly. Line-break
// semantics render the same remark chain through react-markdown.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import ReactMarkdown from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

import type { FileRefSession } from "../../lib/fileRef";

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "vitesse-dark", appearance: "dark" }),
}));

vi.mock("../../lib/snippetHighlighter", () => ({
  highlightSnippet: vi.fn().mockResolvedValue("<pre><code>highlighted</code></pre>"),
}));

interface PrimitiveCall {
  text: string;
  smooth: boolean;
  remarkPlugins: unknown[];
  components: Record<string, React.ComponentType<Record<string, unknown>>>;
}

const primitiveCalls: PrimitiveCall[] = [];

vi.mock("@assistant-ui/react-markdown", () => ({
  MarkdownTextPrimitive: (props: {
    preprocess: () => string;
    smooth?: boolean;
    remarkPlugins?: unknown[];
    className?: string;
    components?: PrimitiveCall["components"];
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

import { Markdown, remarkPluginsFor } from "./Markdown";
import { AcpFileRefContext } from "./AcpFileRefContext";
import { highlightSnippet } from "../../lib/snippetHighlighter";
import type { SyntaxHighlighterProps } from "@assistant-ui/react-markdown";
import { renderWithLateResolution } from "../../__tests__/lateResolution";

beforeEach(() => {
  primitiveCalls.length = 0;
});

afterEach(() => {
  cleanup();
});

/** The override the wrapper registers under `key`. */
function override<P>(key: string): React.ComponentType<P> {
  render(<Markdown text="x" />);
  return primitiveCalls.at(-1)!.components[key] as unknown as React.ComponentType<P>;
}

function renderIn(node: React.ReactNode, ctx: Parameters<typeof AcpFileRefContext.Provider>[0]["value"] = {}) {
  return render(<AcpFileRefContext.Provider value={ctx}>{node}</AcpFileRefContext.Provider>);
}

function click(el: Element) {
  const event = new MouseEvent("click", { bubbles: true, cancelable: true });
  fireEvent(el, event);
  return event;
}

describe("Markdown wrapper", () => {
  it("forwards text, smooth, the remark chain, overrides, and the size-variable class", () => {
    const { container } = render(<Markdown text="hello world" />);
    render(<Markdown text="b" smooth breaks />);
    const [plain, user] = primitiveCalls;
    expect(plain!.text).toBe("hello world");
    expect(plain!.smooth).toBe(false);
    expect(user!.smooth).toBe(true);
    expect(plain!.remarkPlugins).toEqual([remarkGfm]);
    expect(user!.remarkPlugins).toEqual([remarkGfm, remarkBreaks]);
    expect(Object.keys(plain!.components)).toEqual(
      expect.arrayContaining(["SyntaxHighlighter", "CodeHeader", "table", "blockquote", "a", "img"]),
    );
    // A text-size utility would outrank the conversation font-size variable.
    const node = container.querySelector(".acp-markdown") as HTMLElement;
    expect(node.classList.contains("acp-markdown-body")).toBe(true);
    expect([...node.classList].some((c) => /^text-(xs|sm|base|lg|\[)/.test(c))).toBe(false);
  });
});

describe("remark line breaks", () => {
  const renderMd = (text: string, breaks: boolean) =>
    render(<ReactMarkdown remarkPlugins={remarkPluginsFor(breaks)}>{text}</ReactMarkdown>).container;

  it.each([
    ["line a\nline b\nline c", false, 0, 1],
    ["line a\nline b\nline c", true, 2, 1],
    ["para one\n\npara two", true, 0, 2],
  ])("%j breaks=%s renders %i <br> in %i paragraphs", (text, breaks, brs, paragraphs) => {
    const container = renderMd(text, breaks);
    expect(container.querySelectorAll("br")).toHaveLength(brs);
    expect(container.querySelectorAll("p")).toHaveLength(paragraphs);
  });

  it("leaves fenced code intact with breaks on", () => {
    const container = renderMd("```ts\nconst a = 1;\nconst b = 2;\n```", true);
    expect(container.querySelector("pre")?.textContent).toContain("const b = 2;");
    expect(container.querySelectorAll("pre br")).toHaveLength(0);
    expect(container.textContent).not.toContain("```");
  });
});

describe("Blockquote override", () => {
  it.each([
    [<>⚠️ context reset</>, true],
    [<> ⚠️ warning</>, true],
    [
      <span>
        <strong>⚠️</strong> nested warning
      </span>,
      true,
    ],
    [<>just a quote</>, false],
  ])("warning variant %#", (children, warn) => {
    const Blockquote = override<{ children: React.ReactNode }>("blockquote");
    const { container } = render(<Blockquote>{children}</Blockquote>);
    expect(container.querySelector("blockquote")!.className.includes("acp-callout-warn")).toBe(warn);
  });
});

describe("anchor override", () => {
  const session: FileRefSession = {
    id: "s1",
    project_path: "/Users/me/repo",
    main_repo_path: null,
    workspace_repos: [],
  };
  const artSession: FileRefSession = {
    id: "sess-1",
    project_path: "/repo",
    main_repo_path: null,
    workspace_repos: [],
    artifact_dir: "/home/u/.aoe/artifacts/sess-1",
  };
  const IN_REPO = "/Users/me/repo/src/app.ts:42";

  it("opens external links in a new tab with a safe rel, preserving props", () => {
    const Anchor = override<React.ComponentPropsWithoutRef<"a">>("a");
    const onOpenFileRef = vi.fn();
    const { container } = renderIn(
      <Anchor href="https://example.com/path" title="t">
        link text
      </Anchor>,
      { onOpenFileRef, fileRefSession: session },
    );
    const a = container.querySelector("a")!;
    expect(a.getAttribute("href")).toBe("https://example.com/path");
    expect(a.getAttribute("title")).toBe("t");
    expect(a.textContent).toBe("link text");
    expect(a.getAttribute("target")).toBe("_blank");
    expect(a.getAttribute("rel")).toBe("noopener noreferrer");
    expect(click(a).defaultPrevented).toBe(false);
    expect(onOpenFileRef).not.toHaveBeenCalled();
  });

  it.each([
    ["without a session", undefined],
    ["inside the session repo", session],
  ])("routes a local file link to the viewer %s", (_label, fileRefSession) => {
    const Anchor = override<React.ComponentPropsWithoutRef<"a">>("a");
    const onOpenFileRef = vi.fn();
    const { container } = renderIn(<Anchor href={IN_REPO}>app.ts</Anchor>, { onOpenFileRef, fileRefSession });
    expect(click(container.querySelector("a")!).defaultPrevented).toBe(true);
    expect(onOpenFileRef).toHaveBeenCalledWith({ path: "/Users/me/repo/src/app.ts", line: 42 });
  });

  it("falls through to a new-tab anchor for a local link with no handler", () => {
    const Anchor = override<React.ComponentPropsWithoutRef<"a">>("a");
    const { container } = renderIn(<Anchor href={IN_REPO}>app.ts</Anchor>);
    const a = container.querySelector("a")!;
    expect(click(a).defaultPrevented).toBe(false);
    expect(a.getAttribute("target")).toBe("_blank");
  });

  // An out-of-repo file the agent touched is reachable from its tool card instead.
  it("renders an out-of-repo path as inert text", () => {
    const Anchor = override<React.ComponentPropsWithoutRef<"a">>("a");
    const onOpenFileRef = vi.fn();
    const { container } = renderIn(<Anchor href="/tmp/codex-agent-views/shot.png">shot.png</Anchor>, {
      onOpenFileRef,
      fileRefSession: session,
    });
    expect(container.querySelector("a")).toBeNull();
    expect(container.querySelector("span.acp-inert-path")?.textContent).toBe("shot.png");
  });

  it.each([
    ["/aoe/artifacts/shot.png", "/api/sessions/sess-1/artifacts/shot.png"],
    ["/home/u/.aoe/artifacts/sess-1/sub/x.png", "/api/sessions/sess-1/artifacts/sub/x.png"],
  ])("maps artifact path %s to the authenticated route", (href, route) => {
    const Anchor = override<React.ComponentPropsWithoutRef<"a">>("a");
    const { container } = renderIn(<Anchor href={href}>x</Anchor>, {
      onOpenFileRef: vi.fn(),
      fileRefSession: artSession,
    });
    expect(container.querySelector("a.acp-artifact-link")?.getAttribute("href")).toBe(route);
  });

  describe("img", () => {
    const renderImg = (src: string, alt: string) => {
      const Img = override<React.ComponentPropsWithoutRef<"img">>("img");
      return renderIn(<Img src={src} alt={alt} />, { fileRefSession: artSession }).container;
    };

    it("never emits the raw artifact path as an img src", () => {
      expect(
        renderImg("/aoe/artifacts/shot.png", "a shot").querySelector('img[src="/aoe/artifacts/shot.png"]'),
      ).toBeNull();
    });

    it("renders an unresolvable local image as inert alt text", () => {
      const container = renderImg("/tmp/other/x.png", "alt text");
      expect(container.querySelector("img")).toBeNull();
      expect(container.querySelector("span.acp-inert-path")?.textContent).toBe("alt text");
    });

    it("leaves external images alone", () => {
      expect(
        renderImg("https://example.com/x.png", "ext").querySelector('img[src="https://example.com/x.png"]'),
      ).not.toBeNull();
    });
  });
});

describe("table and code header overrides", () => {
  it("wraps tables in a scroll container", () => {
    const Table = override<{ children: React.ReactNode }>("table");
    const { container } = render(
      <Table>
        <tbody>
          <tr>
            <td>cell</td>
          </tr>
        </tbody>
      </Table>,
    );
    expect(container.querySelector(".acp-table-wrap table td")?.textContent).toBe("cell");
  });

  it.each([
    ["rust", "rust"],
    [undefined, "text"],
  ])("labels language %s and copies the raw source", (language, label) => {
    const Header = override<{ language?: string; code: string }>("CodeHeader");
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    const { container, getByText } = render(<Header language={language} code="alert('hi')" />);
    expect(container.textContent).toContain(label);
    fireEvent.click(getByText("copy"));
    expect(writeText).toHaveBeenCalledWith("alert('hi')");
  });
});

describe("ShikiSyntaxHighlighter stale-content transitions (#3974)", () => {
  function getSyntaxHighlighter(): React.ComponentType<SyntaxHighlighterProps> {
    render(<Markdown text="x" />);
    return primitiveCalls.at(-1)!.components.SyntaxHighlighter as React.ComponentType<SyntaxHighlighterProps>;
  }

  afterEach(() => {
    vi.mocked(highlightSnippet).mockReset();
    vi.mocked(highlightSnippet).mockResolvedValue("<pre><code>highlighted</code></pre>");
  });

  it("clears highlighted output when a reused block transitions to unfenced text", async () => {
    vi.mocked(highlightSnippet).mockResolvedValueOnce('<pre class="shiki">highlighted rust</pre>');
    const Comp = getSyntaxHighlighter();

    const { container, rerender } = render(<Comp language="rust" code="fn main() {}" />);
    await waitFor(() => {
      expect(container.querySelector("pre.shiki")).toBeTruthy();
    });

    rerender(<Comp language={undefined} code="plain paragraph text" />);

    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.textContent).toContain("plain paragraph text");
    expect(container.textContent).not.toContain("fn main");
  });

  it("clears highlighted output when a reused block's highlight rejects", async () => {
    vi.mocked(highlightSnippet).mockResolvedValueOnce('<pre class="shiki">highlighted rust</pre>');
    const Comp = getSyntaxHighlighter();

    const { container, rerender } = render(<Comp language="rust" code="fn main() {}" />);
    await waitFor(() => {
      expect(container.querySelector("pre.shiki")).toBeTruthy();
    });

    vi.mocked(highlightSnippet).mockRejectedValueOnce(new Error("boom"));
    rerender(<Comp language="unknownlang" code="fn other() {}" />);

    await waitFor(() => {
      expect(container.textContent).toContain("fn other() {}");
    });
    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.textContent).not.toContain("fn main");
  });

  it("ignores a late resolution from a superseded request (pending A → committed B → late A)", async () => {
    let resolveA!: (v: string | null) => void;
    vi.mocked(highlightSnippet).mockReturnValueOnce(
      new Promise<string | null>((res) => {
        resolveA = res;
      }),
    );
    const Comp = getSyntaxHighlighter();

    const { html, text } = await renderWithLateResolution({
      a: <Comp language="rust" code="fn a() {}" />,
      b: <Comp language={undefined} code="plain b text" />,
      bText: "plain b text",
      resolveStale: () => resolveA('<pre class="shiki">OLD_A</pre>'),
    });

    expect(text).toContain("plain b text");
    expect(html).not.toContain("OLD_A");
  });
});
