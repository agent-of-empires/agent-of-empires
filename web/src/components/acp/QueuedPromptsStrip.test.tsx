// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
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
  opts: {
    onSendNow?: (prompt: QueuedPrompt) => void;
    canSendNow?: boolean;
    sendNowInterrupts?: boolean;
    pendingResume?: boolean;
    waitingFor?: string | null;
    connecting?: boolean;
    agent?: string;
  } = {},
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
        pendingResume={opts.pendingResume ?? false}
        waitingFor={opts.waitingFor}
        connecting={opts.connecting}
        agent={opts.agent ?? "Claude Code"}
      />
    </AgentProfileProvider>,
  );
}

describe("QueuedPromptsStrip", () => {
  it.each<[string, QueuedPrompt[], string]>([
    // The Daemon holds confirmed rows, so they send with no tab open.
    ["Daemon-held rows", [mk("a", "hello")], "Sends when Codex is free, even if you close this tab."],
    // An unconfirmed row lives only in this browser until it reaches the Daemon.
    [
      "a strip with a pending row",
      [mk("a", "hello"), { ...mk("b", "later"), pending: true }],
      "A message isn't sent yet. This browser sends it when you're back online or reopen this chat.",
    ],
  ])("explains %s in a tooltip that opens on keyboard focus", (_label, queued, tip) => {
    renderWithProfile("codex", queued, { agent: "Codex" });
    const header = screen.getByTestId("queued-strip-header");
    expect(header.hasAttribute("title")).toBe(false);
    expect(screen.queryByRole("tooltip")).toBeNull();
    act(() => header.focus());
    expect(document.activeElement).toBe(header);
    const tooltip = screen.getByRole("tooltip");
    expect(tooltip.textContent).toBe(tip);
    expect(tooltip.textContent).not.toMatch(/tab stays open|other devices|Three closed chats/);
  });

  it.each<[string, boolean, string | null, string, boolean?, boolean?]>([
    ["a new Session names the Agent it waits for", true, "Claude Code", "Pending until Claude Code is ready (1)"],
    ["a resumed Session waits to resume", true, null, "Pending until session resumes (1)"],
    ["a running Session just queues", false, "Claude Code", "Queued (1)"],
    ["a running Worker with the connection down keeps a confirmed row queued", true, null, "Queued (1)", true],
    [
      "a running Worker with the connection down holds an unconfirmed row",
      true,
      null,
      "Pending until connected (1)",
      true,
      true,
    ],
  ])("%s", (_label, pendingResume, waitingFor, header, connecting = false, pending = false) => {
    const row = { ...mk("a", "hello"), ...(pending ? { pending } : {}) };
    const { container } = renderWithProfile("claude", [row], { pendingResume, waitingFor, connecting });
    expect(container.textContent).toContain(header);
    expect(container.textContent).not.toContain("Held in this tab");
  });

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

  it.each([
    [false, true, "Send this queued message now", false],
    [true, true, "Stop the current turn and send this queued message", false],
    [false, false, "Send this queued message now", true],
  ])(
    "labels Send now (interrupts=%s, canSendNow=%s) as %s, disabled=%s",
    (sendNowInterrupts, canSendNow, label, disabled) => {
      const btn = renderWithProfile("claude", [mk("a", "go")], { sendNowInterrupts, canSendNow }).getByTestId(
        "queued-send-now",
      );
      expect(btn.getAttribute("aria-label")).toBe(label);
      expect((btn as HTMLButtonElement).disabled).toBe(disabled);
    },
  );

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
