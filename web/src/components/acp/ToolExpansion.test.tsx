// @vitest-environment jsdom
//
// A run folds into a group by re-parenting its cards. The store keeps a card the
// reader opened open across that, and the group opens only when one was.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

vi.mock("../../lib/snippetHighlighter", () => ({
  highlightSnippet: vi.fn().mockResolvedValue(null),
}));

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));

import { AgentProfileProvider } from "../../lib/agentProfileContext";
import type { ToolCardProps } from "./ToolCardChrome";
import { ToolGroupCard } from "./GroupToolCards";
import { TodoGroupCard } from "./TodoCards";
import { ToolCard } from "./ToolCards";
import { ToolDisplayModeProvider, type ToolDensity } from "./ToolDisplayMode";
import { ToolExpansionProvider } from "./ToolExpansion";
import { makeCompletion, makeError, makeToolCall } from "./__fixtures__/toolCalls";

afterEach(cleanup);

const items: (ToolCardProps & { kind: string })[] = ["a", "b", "c"].map((id) => ({
  tool: makeToolCall({
    id,
    name: "Read",
    kind: "read",
    args_preview: JSON.stringify({ file_path: `/tmp/${id}.rs` }),
  }),
  result: makeCompletion({ toolCallId: id, text: `contents-of-${id}` }),
  kind: "read",
}));

function Thread({
  folded,
  density = "detailed",
  cards = items,
}: {
  folded: boolean;
  density?: ToolDensity;
  cards?: typeof items;
}) {
  return (
    <AgentProfileProvider toolKey={null}>
      <ToolDisplayModeProvider density={density}>
        <ToolExpansionProvider>
          {folded ? (
            <ToolGroupCard items={cards} />
          ) : (
            cards.map((i) => <ToolCard key={i.tool.id} tool={i.tool} result={i.result} />)
          )}
        </ToolExpansionProvider>
      </ToolDisplayModeProvider>
    </AgentProfileProvider>
  );
}

const header = (name: RegExp) => screen.getAllByRole("button", { name })[0]!;

describe("folding a run into a group", () => {
  it.each<ToolDensity>(["detailed", "compact"])("keeps a card the reader opened visible (%s)", (density) => {
    const { container, rerender } = render(<Thread folded={false} density={density} />);
    expect(container.textContent).not.toContain("contents-of-b");
    fireEvent.click(header(/b\.rs/));
    expect(container.textContent).toContain("contents-of-b");

    rerender(<Thread folded density={density} />);
    expect(container.textContent).toContain("3 actions");
    expect(container.textContent).toContain("contents-of-b");
    expect(container.textContent).not.toContain("contents-of-a");
  });

  it("folds to the collapsed group when nothing was open", () => {
    const { container, rerender } = render(<Thread folded={false} />);
    rerender(<Thread folded />);
    expect(container.textContent).toContain("3 actions");
    expect(container.textContent).not.toContain("b.rs");
  });

  it("keeps the group open when the reader collapses its last open card", () => {
    const { container, rerender } = render(<Thread folded={false} />);
    fireEvent.click(header(/b\.rs/));
    rerender(<Thread folded />);
    fireEvent.click(header(/b\.rs/));
    expect(container.textContent).toContain("b.rs");
    expect(container.textContent).toContain("a.rs");
  });

  it("lets the reader collapse the opened card after the fold", () => {
    const { container, rerender } = render(<Thread folded={false} />);
    fireEvent.click(header(/b\.rs/));
    rerender(<Thread folded />);
    fireEvent.click(header(/b\.rs/));
    expect(container.textContent).not.toContain("contents-of-b");
  });

  describe("a failed card, open without any toggle", () => {
    const failing = items.map((i) =>
      i.tool.id === "b" ? { ...i, result: makeError({ toolCallId: "b", text: "boom-b" }) } : i,
    );

    it.each<ToolDensity>(["detailed", "compact"])("keeps it visible (%s)", (density) => {
      const { container, rerender } = render(<Thread folded={false} density={density} cards={failing} />);
      expect(container.textContent).toContain("boom-b");
      rerender(<Thread folded density={density} cards={failing} />);
      expect(container.textContent).toContain("3 actions");
      expect(container.textContent).toContain("boom-b");
    });

    it("stays visible when the density flips after the fold", () => {
      const { container, rerender } = render(<Thread folded cards={failing} />);
      expect(container.textContent).toContain("boom-b");
      rerender(<Thread folded density="compact" cards={failing} />);
      expect(container.textContent).toContain("boom-b");
    });

    it("opens the group when a card still running at the fold fails afterwards", () => {
      const running = items.map((i) => (i.tool.id === "b" ? { ...i, result: undefined } : i));
      const { container, rerender } = render(<Thread folded cards={running} />);
      expect(container.textContent).toContain("3 actions");
      expect(container.textContent).not.toContain("b.rs");
      rerender(<Thread folded cards={failing} />);
      expect(container.textContent).toContain("boom-b");
    });

    it("folds collapsed when the reader had closed it", () => {
      const { container, rerender } = render(<Thread folded={false} cards={failing} />);
      fireEvent.click(header(/b\.rs/));
      expect(container.textContent).not.toContain("boom-b");
      rerender(<Thread folded cards={failing} />);
      expect(container.textContent).toContain("3 actions");
      expect(container.textContent).not.toContain("b.rs");
    });
  });
});

describe("folding todo snapshots into a group", () => {
  // Six items: closed by default, so only the reader's click opens one.
  const snapshots = ["x", "y", "z"].map((id) => ({
    tool: makeToolCall({
      id,
      name: "TodoWrite",
      args_preview: JSON.stringify({
        todos: Array.from({ length: 6 }, (_, n) => ({ content: `item-${id}-${n}`, status: "pending" })),
      }),
    }),
    result: makeCompletion({ toolCallId: id }),
  }));

  function TodoThread({ folded }: { folded: boolean }) {
    return (
      <AgentProfileProvider toolKey="claude">
        <ToolDisplayModeProvider density="detailed">
          <ToolExpansionProvider>
            {folded ? (
              <TodoGroupCard items={snapshots} />
            ) : (
              snapshots.map((s) => <ToolCard key={s.tool.id} tool={s.tool} result={s.result} />)
            )}
          </ToolExpansionProvider>
        </ToolDisplayModeProvider>
      </AgentProfileProvider>
    );
  }

  it("keeps an update the reader opened visible, and folds collapsed otherwise", () => {
    const { container, rerender } = render(<TodoThread folded={false} />);
    expect(container.textContent).not.toContain("item-y-0");
    fireEvent.click(screen.getAllByRole("button", { name: /6 items/ })[1]!);
    expect(container.textContent).toContain("item-y-0");

    rerender(<TodoThread folded />);
    expect(container.textContent).toContain("updated 3 times");
    expect(container.textContent).toContain("item-y-0");
    expect(container.textContent).not.toContain("item-x-0");
  });

  it("folds collapsed when no update was opened", () => {
    const { container, rerender } = render(<TodoThread folded={false} />);
    rerender(<TodoThread folded />);
    expect(container.textContent).toContain("updated 3 times");
    expect(container.textContent).not.toContain("item-y-0");
  });

  it("keeps an update's own toggle across a rerender of the group", () => {
    const { container, rerender } = render(<TodoThread folded />);
    fireEvent.click(screen.getAllByRole("button")[0]!);
    fireEvent.click(screen.getAllByRole("button", { name: /6 items/ })[0]!);
    expect(container.textContent).toContain("item-x-0");
    rerender(<TodoThread folded />);
    expect(container.textContent).toContain("item-x-0");
  });
});
