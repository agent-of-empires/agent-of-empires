// @vitest-environment jsdom
//
// Render coverage for the QueuedPromptsStrip clear-boundary divider
// (#1356). The strip lifts up a visual hint when the queued items will
// fire as separate POSTs because one of them is a clear-command alias.

import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { AgentProfileProvider } from "../../lib/agentProfileContext";
import type { QueuedPrompt } from "../../lib/acpTypes";
import { QueuedPromptsStrip } from "./StructuredView";

function mk(id: string, text: string): QueuedPrompt {
  return { id, text, queuedAt: "2026-05-21T00:00:00.000Z" };
}

function renderWithProfile(
  toolKey: string,
  queued: QueuedPrompt[],
) {
  return render(
    <AgentProfileProvider toolKey={toolKey}>
      <QueuedPromptsStrip
        queued={queued}
        onRemove={() => {}}
        onEdit={() => {}}
        onClear={() => {}}
      />
    </AgentProfileProvider>,
  );
}

describe("QueuedPromptsStrip clear-boundary divider (#1356)", () => {
  afterEach(() => {
    cleanup();
  });

  it("renders a divider above a queued /clear under the claude profile", () => {
    // Two-entry queue stays under the desktop visibleDefault=2 collapse
    // threshold so both rows render without an expand click.
    const { queryAllByTestId } = renderWithProfile("claude", [
      mk("a", "first"),
      mk("c", "/clear"),
    ]);
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(1);
  });

  it("renders a divider below a queued /clear when it leads the visible queue", () => {
    const { queryAllByTestId } = renderWithProfile("claude", [
      mk("c", "/clear"),
      mk("b", "second"),
    ]);
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(1);
  });

  it("renders no divider when the queue contains no clear-command aliases", () => {
    const { queryAllByTestId } = renderWithProfile("claude", [
      mk("a", "first"),
      mk("b", "second"),
    ]);
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(0);
  });

  it("renders nothing when the queue is empty", () => {
    const { container } = renderWithProfile("claude", []);
    expect(container.firstChild).toBeNull();
  });

  it("does not render a divider for an agent profile without clear aliases (gemini)", () => {
    const { queryAllByTestId } = renderWithProfile("gemini", [
      mk("a", "first"),
      mk("c", "/clear"),
    ]);
    // gemini's clearAliases are empty; even with `/clear` text in the
    // queue, the strip should not show a boundary because the agent
    // does not honour `/clear`.
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(0);
  });

  it("treats `/new` as a boundary under the codex profile", () => {
    const { queryAllByTestId } = renderWithProfile("codex", [
      mk("a", "first"),
      mk("n", "/new"),
    ]);
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(1);
  });

  it("treats a `/clear` invocation with trailing args as a boundary", () => {
    const { queryAllByTestId } = renderWithProfile("claude", [
      mk("a", "first"),
      mk("c", "/clear --hard"),
    ]);
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(1);
  });

  it("renders the Clear all button when the queue has more than one entry", () => {
    const { getByRole } = renderWithProfile("claude", [
      mk("a", "first"),
      mk("b", "second"),
    ]);
    expect(getByRole("button", { name: /clear all/i })).toBeTruthy();
  });

  it("omits the Clear all button when the queue has exactly one entry", () => {
    const { queryByRole } = renderWithProfile("claude", [mk("a", "only")]);
    expect(queryByRole("button", { name: /clear all/i })).toBeNull();
  });
});

describe("QueuedPromptRow attachment indicator (#1833)", () => {
  afterEach(() => {
    cleanup();
  });

  const withImage: QueuedPrompt = {
    id: "img",
    text: "look at this",
    queuedAt: "2026-05-21T00:00:00.000Z",
    attachments: [
      {
        kind: "image",
        mimeType: "image/png",
        dataB64: "aA==",
        name: "shot.png",
      },
    ],
  };

  it("renders a thumbnail for an image attachment on a queued row", () => {
    const { getByTestId, getByAltText } = renderWithProfile("claude", [
      withImage,
    ]);
    expect(getByTestId("queued-attachments")).toBeTruthy();
    const img = getByAltText("shot.png") as HTMLImageElement;
    expect(img.src).toContain("data:image/png;base64,aA==");
  });

  it("renders no attachment strip for a text-only queued row", () => {
    const { queryByTestId } = renderWithProfile("claude", [
      mk("a", "plain text"),
    ]);
    expect(queryByTestId("queued-attachments")).toBeNull();
  });
});

describe("QueuedPromptRow expanded-state bounds (#1642)", () => {
  afterEach(() => {
    cleanup();
  });

  // A multi-thousand-char paste: trips isQueuedPromptLong (>160 chars) so
  // the row gets the clamp + "…" affordance.
  const hugePaste = "lorem ipsum ".repeat(500);

  it("clamps a long collapsed prompt and offers the expand affordance", () => {
    const { getByTitle, getByRole } = renderWithProfile("claude", [
      mk("a", hugePaste),
    ]);
    const textButton = getByTitle("Click to edit");
    expect(textButton.className).toContain("line-clamp-3");
    // `block` must NOT co-exist with `line-clamp-3`: its `display:block`
    // wins the cascade over line-clamp's `-webkit-box` and silently kills
    // the clamp, rendering the whole paste. See #1642.
    expect(textButton.className.split(/\s+/)).not.toContain("block");
    // Collapsed affordance is the "…" toggle.
    expect(
      getByRole("button", { name: "Show full queued prompt" }),
    ).toBeTruthy();
  });

  it("bounds the expanded prompt to a scrollable box so it cannot grow the strip", () => {
    const { getByTitle, getByRole } = renderWithProfile("claude", [
      mk("a", hugePaste),
    ]);
    fireEvent.click(getByRole("button", { name: "Show full queued prompt" }));

    // Clamp is gone once expanded.
    const textButton = getByTitle("Click to edit");
    expect(textButton.className).not.toContain("line-clamp-3");

    // The text now lives inside a height-capped, scrollable wrapper, so a
    // huge paste scrolls in place instead of pushing the composer off-screen.
    const box = textButton.parentElement as HTMLElement;
    expect(box.className).toContain("max-h-48");
    expect(box.className).toContain("overflow-y-auto");
  });

  it("keeps the Show less toggle outside the scrollable region so it stays reachable", () => {
    const { getByTitle, getByRole } = renderWithProfile("claude", [
      mk("a", hugePaste),
    ]);
    fireEvent.click(getByRole("button", { name: "Show full queued prompt" }));

    const collapseToggle = getByRole("button", {
      name: "Collapse queued prompt",
    });
    expect(collapseToggle.textContent).toBe("Show less");

    // The toggle must be a sibling of the capped box, not a descendant of
    // it; otherwise it would scroll away with the paste and get buried.
    const box = getByTitle("Click to edit").parentElement as HTMLElement;
    expect(box.contains(collapseToggle)).toBe(false);
  });
});
