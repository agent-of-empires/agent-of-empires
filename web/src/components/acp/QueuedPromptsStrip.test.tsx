// @vitest-environment jsdom
import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { AgentProfileProvider } from "../../lib/agentProfileContext";
import type { QueuedPrompt } from "../../lib/acpTypes";
import { QueuedPromptsStrip } from "./PromptStrips";

afterEach(() => {
  cleanup();
});

function mk(id: string, text: string): QueuedPrompt {
  return { id, text, queuedAt: "2026-05-21T00:00:00.000Z" };
}

// Server-owned clear aliases per agent, as `SessionResponse.clear_aliases` resolves them.
const SERVER_CLEAR_ALIASES: Record<string, string[]> = {
  claude: ["/clear"],
  codex: ["/new"],
  gemini: [],
};

function renderWithProfile(
  toolKey: string,
  queued: QueuedPrompt[],
  opts: { onSendNow?: (prompt: QueuedPrompt) => void; canSendNow?: boolean; sendNowInterrupts?: boolean } = {},
) {
  return render(
    <AgentProfileProvider toolKey={toolKey} clearAliases={SERVER_CLEAR_ALIASES[toolKey] ?? []}>
      <QueuedPromptsStrip
        queued={queued}
        onRemove={() => {}}
        onEdit={() => {}}
        onClear={() => {}}
        onSendNow={opts.onSendNow ?? (() => {})}
        canSendNow={opts.canSendNow ?? true}
        sendNowInterrupts={opts.sendNowInterrupts ?? false}
        pendingResume={false}
      />
    </AgentProfileProvider>,
  );
}

describe("QueuedPromptsStrip", () => {
  // Two entries stay under the desktop collapse threshold, so both rows render.
  it.each([
    ["claude", ["first", "/clear"], 1],
    ["claude", ["/clear", "second"], 1],
    ["claude", ["first", "second"], 0],
    ["claude", ["first", "/clear --hard"], 1],
    ["codex", ["first", "/new"], 1],
    // gemini has no clear aliases, so `/clear` text is not a boundary.
    ["gemini", ["first", "/clear"], 0],
  ])("%s queue %o renders %i clear-boundary dividers", (tool, texts, dividers) => {
    const { queryAllByTestId } = renderWithProfile(
      tool,
      texts.map((t, i) => mk(String(i), t)),
    );
    expect(queryAllByTestId("queued-clear-boundary")).toHaveLength(dividers);
  });

  it("renders nothing when the queue is empty", () => {
    expect(renderWithProfile("claude", []).container.firstChild).toBeNull();
  });

  it.each([
    [["only"], false],
    [["first", "second"], true],
  ])("Clear all for %o: %s", (texts, shown) => {
    const { queryByRole } = renderWithProfile(
      "claude",
      texts.map((t, i) => mk(String(i), t)),
    );
    expect(queryByRole("button", { name: /clear all/i }) !== null).toBe(shown);
  });

  it("force-sends the row's prompt when Send now is clicked", () => {
    const sent: QueuedPrompt[] = [];
    const row = mk("a", "ship it");
    const { getByTestId } = renderWithProfile("claude", [row], { onSendNow: (p) => sent.push(p) });
    fireEvent.click(getByTestId("queued-send-now"));
    expect(sent).toEqual([row]);
  });

  it("disables Send now when the session is down", () => {
    const { getByTestId } = renderWithProfile("claude", [mk("a", "wait")], { canSendNow: false });
    expect((getByTestId("queued-send-now") as HTMLButtonElement).disabled).toBe(true);
  });

  it.each([
    [false, "Send this queued message now"],
    [true, "Stop the current turn and send this queued message"],
  ])("labels Send now (interrupts=%s) and keeps it pressable", (sendNowInterrupts, label) => {
    const btn = renderWithProfile("claude", [mk("a", "go")], { sendNowInterrupts }).getByTestId("queued-send-now");
    expect(btn.getAttribute("aria-label")).toBe(label);
    expect((btn as HTMLButtonElement).disabled).toBe(false);
  });

  it("renders a thumbnail for an image attachment and no strip for text-only rows", () => {
    const withImage: QueuedPrompt = {
      ...mk("img", "look at this"),
      attachments: [{ kind: "image", mimeType: "image/png", dataB64: "aA==", name: "shot.png" }],
    };
    const { getByTestId, getByAltText } = renderWithProfile("claude", [withImage]);
    expect(getByTestId("queued-attachments")).toBeTruthy();
    expect((getByAltText("shot.png") as HTMLImageElement).src).toContain("data:image/png;base64,aA==");
    cleanup();
    expect(renderWithProfile("claude", [mk("a", "plain text")]).queryByTestId("queued-attachments")).toBeNull();
  });

  it("clamps a long prompt, then bounds the expanded text in a scroll box with the toggle outside it", () => {
    const { getByTitle, getByRole } = renderWithProfile("claude", [mk("a", "lorem ipsum ".repeat(500))]);
    const textButton = getByTitle("Click to edit");
    expect(textButton.className).toContain("line-clamp-3");
    // `block` would override line-clamp's -webkit-box display and render the whole paste.
    expect(textButton.className.split(/\s+/)).not.toContain("block");

    fireEvent.click(getByRole("button", { name: "Show full queued prompt" }));
    expect(getByTitle("Click to edit").className).not.toContain("line-clamp-3");
    const box = getByTitle("Click to edit").parentElement as HTMLElement;
    expect(box.className).toContain("max-h-48");
    expect(box.className).toContain("overflow-y-auto");
    const collapseToggle = getByRole("button", { name: "Collapse queued prompt" });
    expect(collapseToggle.textContent).toBe("Show less");
    expect(box.contains(collapseToggle)).toBe(false);
  });
});
