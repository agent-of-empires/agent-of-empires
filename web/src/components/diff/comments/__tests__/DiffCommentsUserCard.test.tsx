// @vitest-environment jsdom
//
// Render tests for DiffCommentsUserCard, the rich diff-review prompt card in
// the structured-view user-message slot. Covers the comment count label, the
// optional intro/outro framing, the stable sort across repo/file/line/side,
// the single-line vs range header wording, and the multi-repo repo chip. The
// Shiki highlighter is mocked so the snippet renderer stays deterministic and
// the test never touches the network or leaves async work pending; with no
// resolved HTML the card falls back to a plain <pre>, exercising that path.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";

import { DiffCommentsUserCard } from "../../comments/DiffCommentsUserCard";
import type { DiffCommentsCardPayload } from "../../comments/buildPrompt";
import type { DiffComment } from "../../comments/types";
import { renderWithLateResolution } from "../../../../__tests__/lateResolution";

// Test-controlled highlighter behavior. By default `highlightSnippet`
// resolves to null, so HighlightedSnippet caches nothing and renders
// the plain <pre> fallback branch. Flipping `highlighterMock.loaded` on
// exercises the resolved-HTML (dangerouslySetInnerHTML) branch. Setting
// `highlighterMock.deferred` hands the next call a manually-resolved promise
// for the superseded-request ordering test. No network or real Shiki
// involvement, so the suite stays deterministic.
const highlighterMock = { loaded: false, deferred: null as Promise<string | null> | null };

vi.mock("../../../../lib/snippetHighlighter", () => ({
  highlightSnippet: (code: string) => {
    if (highlighterMock.deferred) {
      const p = highlighterMock.deferred;
      highlighterMock.deferred = null;
      return p;
    }
    return Promise.resolve(highlighterMock.loaded ? `<pre class="shiki"><code>${code}</code></pre>` : null);
  },
  DEFAULT_SHIKI_THEME: "github-dark",
}));

function comment(overrides: Partial<DiffComment> = {}): DiffComment {
  return {
    id: overrides.id ?? "c1",
    repoName: overrides.repoName,
    filePath: overrides.filePath ?? "src/app.ts",
    side: overrides.side ?? "new",
    startLine: overrides.startLine ?? 10,
    endLine: overrides.endLine ?? 10,
    body: overrides.body ?? "looks good",
    capturedSnippet: overrides.capturedSnippet ?? "const x = 1;",
    language: overrides.language,
    createdAt: overrides.createdAt ?? "2026-01-01T00:00:00Z",
    updatedAt: overrides.updatedAt,
  };
}

function payload(overrides: Partial<DiffCommentsCardPayload> = {}): DiffCommentsCardPayload {
  return {
    intro: overrides.intro ?? "",
    outro: overrides.outro ?? "",
    isMultiRepo: overrides.isMultiRepo ?? false,
    comments: overrides.comments ?? [comment()],
  };
}

beforeEach(() => {
  highlighterMock.loaded = false;
  highlighterMock.deferred = null;
});

afterEach(() => {
  cleanup();
});

describe("DiffCommentsUserCard", () => {
  it("renders the diff review badge and a singular comment count", () => {
    const { container } = render(<DiffCommentsUserCard payload={payload()} />);
    expect(container.textContent).toContain("diff review");
    // The count label is rendered as "1 comment" with the singular suffix; the
    // plural "s" is only appended when count !== 1. Read the count node text
    // directly so an adjacent "src/..." path can't bleed into a substring match.
    const countText = Array.from(container.querySelectorAll("span")).find((s) =>
      /^\d+ comment(s)?$/.test(s.textContent ?? ""),
    )?.textContent;
    expect(countText).toBe("1 comment");
  });

  it("pluralizes the comment count for multiple comments", () => {
    const { container } = render(
      <DiffCommentsUserCard
        payload={payload({ comments: [comment({ id: "a" }), comment({ id: "b", filePath: "src/b.ts" })] })}
      />,
    );
    expect(container.textContent).toContain("2 comments");
  });

  it("renders the comment body, file path, and snippet in the plain-text fallback", () => {
    const { container } = render(
      <DiffCommentsUserCard
        payload={payload({ comments: [comment({ body: "fix this", filePath: "src/widget.ts" })] })}
      />,
    );
    expect(container.textContent).toContain("fix this");
    expect(container.textContent).toContain("src/widget.ts");
    const pre = container.querySelector("pre");
    expect(pre?.textContent).toBe("const x = 1;");
  });

  it("renders a single-line range as 'line N' and a multi-line range as 'lines N-M'", () => {
    const single = render(
      <DiffCommentsUserCard payload={payload({ comments: [comment({ startLine: 5, endLine: 5 })] })} />,
    );
    expect(single.container.textContent).toContain("line 5");
    expect(single.container.textContent).not.toContain("lines 5");
    cleanup();
    const range = render(
      <DiffCommentsUserCard payload={payload({ comments: [comment({ startLine: 5, endLine: 9 })] })} />,
    );
    expect(range.container.textContent).toContain("lines 5-9");
  });

  it("shows the comment side in the header", () => {
    const { container } = render(<DiffCommentsUserCard payload={payload({ comments: [comment({ side: "old" })] })} />);
    expect(container.textContent).toContain("old");
  });

  it("renders intro and outro framing when present", () => {
    const { container } = render(
      <DiffCommentsUserCard payload={payload({ intro: "Please review", outro: "Thanks!" })} />,
    );
    expect(container.textContent).toContain("Please review");
    expect(container.textContent).toContain("Thanks!");
  });

  it("omits intro and outro blocks when blank", () => {
    const { container } = render(<DiffCommentsUserCard payload={payload({ intro: "", outro: "" })} />);
    // Only the snippet/body content should be present; no extra framing blocks.
    const framingBlocks = container.querySelectorAll(".border-l-2");
    expect(framingBlocks.length).toBe(0);
  });

  it("shows the repo chip only when isMultiRepo and repoName are set", () => {
    const single = render(
      <DiffCommentsUserCard payload={payload({ isMultiRepo: false, comments: [comment({ repoName: "frontend" })] })} />,
    );
    expect(single.container.textContent).not.toContain("frontend");
    cleanup();
    const multi = render(
      <DiffCommentsUserCard payload={payload({ isMultiRepo: true, comments: [comment({ repoName: "frontend" })] })} />,
    );
    expect(multi.container.textContent).toContain("frontend");
  });

  it("sorts comments by repo, file, then start line", () => {
    const comments: DiffComment[] = [
      comment({ id: "1", repoName: "z", filePath: "a.ts", startLine: 1, endLine: 1 }),
      comment({ id: "2", repoName: "a", filePath: "b.ts", startLine: 30, endLine: 30 }),
      comment({ id: "3", repoName: "a", filePath: "b.ts", startLine: 5, endLine: 5 }),
    ];
    const { container } = render(<DiffCommentsUserCard payload={payload({ isMultiRepo: true, comments })} />);
    const items = Array.from(container.querySelectorAll("li"));
    expect(items).toHaveLength(3);
    // repo "a" sorts before "z"; within repo "a"/file "b.ts", line 5 before 30.
    expect(items[0].textContent).toContain("line 5");
    expect(items[1].textContent).toContain("line 30");
    expect(items[2].textContent).toContain("line 1");
  });

  it("renders the highlighted HTML snippet when the language resolves", async () => {
    highlighterMock.loaded = true;
    const { container } = render(
      <DiffCommentsUserCard
        payload={payload({ comments: [comment({ capturedSnippet: "const y = 2;", language: "typescript" })] })}
      />,
    );
    await waitFor(() => expect(container.querySelector("pre.shiki")).toBeTruthy());
    expect(container.querySelector("pre.shiki")?.textContent).toContain("const y = 2;");
  });

  it("clears highlighted output when the same comment slot is reused with an unresolved language (#3974)", async () => {
    highlighterMock.loaded = true;
    const { container, rerender } = render(
      <DiffCommentsUserCard
        payload={payload({
          comments: [comment({ id: "c1", capturedSnippet: "const y = 2;", language: "typescript" })],
        })}
      />,
    );
    await waitFor(() => expect(container.querySelector("pre.shiki")).toBeTruthy());

    highlighterMock.loaded = false;
    rerender(
      <DiffCommentsUserCard
        payload={payload({
          comments: [comment({ id: "c1", capturedSnippet: "plain text body", language: undefined, filePath: "NOTES" })],
        })}
      />,
    );

    expect(container.querySelector("pre.shiki")).toBeNull();
    const pre = container.querySelector("pre");
    expect(pre?.textContent).toBe("plain text body");
  });

  it("ignores a late resolution from a superseded request (pending A → committed B → late A)", async () => {
    let resolveA!: (v: string | null) => void;
    highlighterMock.deferred = new Promise<string | null>((res) => {
      resolveA = res;
    });
    const card = (c: DiffComment) => <DiffCommentsUserCard payload={payload({ comments: [c] })} />;

    const { html, text } = await renderWithLateResolution({
      a: card(comment({ id: "c1", capturedSnippet: "const a = 1;", language: "typescript" })),
      b: card(comment({ id: "c1", capturedSnippet: "plain b body", language: undefined, filePath: "NOTES" })),
      bText: "plain b body",
      resolveStale: () => resolveA('<pre class="shiki">OLD_A</pre>'),
    });

    expect(text).toContain("plain b body");
    expect(html).not.toContain("OLD_A");
  });

  it("renders an empty list with a zero-comment count", () => {
    const { container } = render(<DiffCommentsUserCard payload={payload({ comments: [] })} />);
    expect(container.textContent).toContain("0 comments");
    expect(container.querySelectorAll("li")).toHaveLength(0);
  });
});
