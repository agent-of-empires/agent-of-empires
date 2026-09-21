// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";

import { DiffCommentsUserCard } from "../../comments/DiffCommentsUserCard";
import type { DiffCommentsCardPayload } from "../../comments/buildPrompt";
import type { DiffComment } from "../../comments/types";
import { renderWithLateResolution } from "../../../../__tests__/lateResolution";

// `deferred` hands the next call a manually-settled promise, for the
// superseded-request ordering test; otherwise `loaded` picks between the
// resolved-HTML branch and the plain <pre> fallback.
const highlighter = vi.hoisted(() => ({ loaded: false, deferred: null as Promise<string | null> | null }));
vi.mock("../../../../lib/snippetHighlighter", () => ({
  highlightSnippet: (code: string) => {
    const pending = highlighter.deferred;
    if (pending) {
      highlighter.deferred = null;
      return pending;
    }
    return Promise.resolve(highlighter.loaded ? `<pre class="shiki"><code>${code}</code></pre>` : null);
  },
  DEFAULT_SHIKI_THEME: "github-dark",
}));

const comment = (over: Partial<DiffComment> = {}): DiffComment => ({
  id: "c1",
  filePath: "src/app.ts",
  side: "new",
  startLine: 10,
  endLine: 10,
  body: "looks good",
  capturedSnippet: "const x = 1;",
  createdAt: "2026-01-01T00:00:00Z",
  ...over,
});

const renderCard = (over: Partial<DiffCommentsCardPayload> = {}) =>
  render(
    <DiffCommentsUserCard payload={{ intro: "", outro: "", isMultiRepo: false, comments: [comment()], ...over }} />,
  ).container;

afterEach(() => {
  cleanup();
  highlighter.loaded = false;
  highlighter.deferred = null;
});

describe("DiffCommentsUserCard", () => {
  it.each([
    [[comment()], "1 comment"],
    [[comment({ id: "a" }), comment({ id: "b", filePath: "src/b.ts" })], "2 comments"],
    [[], "0 comments"],
  ])("labels the badge count", (comments, label) => {
    const c = renderCard({ comments });
    expect(c.textContent).toContain("diff review");
    const count = Array.from(c.querySelectorAll("span")).find((s) => /^\d+ comments?$/.test(s.textContent ?? ""));
    expect(count?.textContent).toBe(label);
    expect(c.querySelectorAll("li")).toHaveLength(comments.length);
  });

  it("renders body, path, side and plain snippet, without framing when blank", () => {
    const c = renderCard({ comments: [comment({ body: "fix this", filePath: "src/widget.ts", side: "old" })] });
    for (const t of ["fix this", "src/widget.ts", "old"]) expect(c.textContent).toContain(t);
    expect(c.querySelector("pre")?.textContent).toBe("const x = 1;");
    expect(c.querySelectorAll(".border-l-2")).toHaveLength(0);
  });

  it.each([
    [5, 5, "line 5", "lines 5"],
    [5, 9, "lines 5-9", undefined],
  ])("renders range %i-%i", (startLine, endLine, shown, hidden) => {
    const c = renderCard({ comments: [comment({ startLine, endLine })] });
    expect(c.textContent).toContain(shown);
    if (hidden) expect(c.textContent).not.toContain(hidden);
  });

  it("renders intro and outro framing", () => {
    const c = renderCard({ intro: "Please review", outro: "Thanks!" });
    expect(c.textContent).toContain("Please review");
    expect(c.textContent).toContain("Thanks!");
  });

  it.each([false, true])("shows the repo chip only for multi-repo (%s)", (isMultiRepo) => {
    const c = renderCard({ isMultiRepo, comments: [comment({ repoName: "frontend" })] });
    expect(c.textContent?.includes("frontend")).toBe(isMultiRepo);
  });

  it("sorts by repo, file, then start line", () => {
    const c = renderCard({
      isMultiRepo: true,
      comments: [
        comment({ id: "1", repoName: "z", filePath: "a.ts", startLine: 1, endLine: 1 }),
        comment({ id: "2", repoName: "a", filePath: "b.ts", startLine: 30, endLine: 30 }),
        comment({ id: "3", repoName: "a", filePath: "b.ts", startLine: 5, endLine: 5 }),
      ],
    });
    const items = Array.from(c.querySelectorAll("li")).map((li) => li.textContent);
    expect(items[0]).toContain("line 5");
    expect(items[1]).toContain("line 30");
    expect(items[2]).toContain("line 1");
  });

  it("renders highlighted HTML when the language resolves", async () => {
    highlighter.loaded = true;
    const c = renderCard({ comments: [comment({ capturedSnippet: "const y = 2;", language: "typescript" })] });
    await waitFor(() => expect(c.querySelector("pre.shiki")?.textContent).toContain("const y = 2;"));
  });

  it("clears highlighted output when the same slot is reused with an unresolved language (#3974)", async () => {
    highlighter.loaded = true;
    const card = (over: Partial<DiffComment>) => (
      <DiffCommentsUserCard
        payload={{ intro: "", outro: "", isMultiRepo: false, comments: [comment({ id: "c1", ...over })] }}
      />
    );
    const { container, rerender } = render(card({ capturedSnippet: "const y = 2;", language: "typescript" }));
    await waitFor(() => expect(container.querySelector("pre.shiki")).toBeTruthy());

    highlighter.loaded = false;
    rerender(card({ capturedSnippet: "plain text body", language: undefined, filePath: "NOTES" }));

    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.querySelector("pre")?.textContent).toBe("plain text body");
  });

  it("ignores a late resolution from a superseded request (pending A -> committed B -> late A)", async () => {
    let resolveA!: (v: string | null) => void;
    highlighter.deferred = new Promise<string | null>((res) => {
      resolveA = res;
    });
    const card = (over: Partial<DiffComment>) => (
      <DiffCommentsUserCard
        payload={{ intro: "", outro: "", isMultiRepo: false, comments: [comment({ id: "c1", ...over })] }}
      />
    );

    const { html, text } = await renderWithLateResolution({
      a: card({ capturedSnippet: "const a = 1;", language: "typescript" }),
      b: card({ capturedSnippet: "plain b body", language: undefined, filePath: "NOTES" }),
      bText: "plain b body",
      resolveStale: () => resolveA('<pre class="shiki">OLD_A</pre>'),
    });

    expect(text).toContain("plain b body");
    expect(html).not.toContain("OLD_A");
  });
});
