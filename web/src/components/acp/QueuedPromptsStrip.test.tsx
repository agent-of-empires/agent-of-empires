// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { AgentProfileProvider } from "../../lib/agentProfileContext";
import type { QueuedPrompt } from "../../lib/acpTypes";
import { QueuedPromptsStrip } from "./PromptStrips";
import type { QueuedReason } from "./queuedReason";

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
const SERVER_COMPACT_ALIASES: Record<string, string[]> = { claude: ["/compact"] };

const IDLE: QueuedReason = { label: "Queued", reason: null, action: "send" };

function renderWithProfile(
  toolKey: string,
  queued: QueuedPrompt[],
  opts: {
    onSendNow?: (prompt: QueuedPrompt) => void;
    canSendNow?: boolean;
    onStartAndSend?: () => void;
    reason?: QueuedReason;
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
        onStartAndSend={opts.onStartAndSend}
        reason={opts.reason ?? IDLE}
        soloAliases={[...(SERVER_CLEAR_ALIASES[toolKey] ?? []), ...(SERVER_COMPACT_ALIASES[toolKey] ?? [])]}
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

  it.each<[string, QueuedReason, string, string | null]>([
    [
      "a held queue names why",
      { label: "Queued", reason: "sends when Codex finishes", action: "stopAndSend" },
      "Queued (1)",
      "· sends when Codex finishes",
    ],
    ["an idle queue has no reason", IDLE, "Queued (1)", null],
    // Offline with a row the Daemon never got: nothing has been sent yet.
    [
      "an offline pending row",
      { label: "Not sent", reason: "sends when this dashboard reconnects", action: "dropOnly" },
      "Not sent (1)",
      "· sends when this dashboard reconnects",
    ],
  ])("%s", (_label, reason, header, reasonText) => {
    const { getByTestId, queryByTestId } = renderWithProfile("claude", [mk("a", "hello")], { reason });
    const headerEl = getByTestId("queued-strip-header");
    expect(headerEl.textContent?.startsWith(header)).toBe(true);
    expect(queryByTestId("queued-strip-reason")?.textContent ?? null).toBe(reasonText);
    // The header reads at the validated subtle contrast, not the dimmer token.
    expect(headerEl.closest("div")?.outerHTML).not.toContain("text-text-dim");
  });

  // Two entries stay under the desktop collapse threshold, so both rows render.
  it.each([
    ["claude", ["first", "/clear"], 1],
    ["claude", ["/clear", "second"], 1],
    ["claude", ["first", "second"], 0],
    ["claude", ["first", "/clear --hard"], 1],
    // The drain runs a compact command on its own, like a clear.
    ["claude", ["first", "/compact"], 1],
    ["codex", ["first", "/new"], 1],
    // gemini has no clear aliases, so `/clear` text is not a boundary.
    ["gemini", ["first", "/clear"], 0],
  ])("%s queue %o renders %i solo-command dividers", (tool, texts, dividers) => {
    const { queryAllByTestId } = renderWithProfile(
      tool,
      texts.map((t, i) => mk(String(i), t)),
    );
    expect(queryAllByTestId("queued-solo-boundary")).toHaveLength(dividers);
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

  it.each<[QueuedReason["action"], string, boolean, string | null]>([
    ["send", "Send this queued message now", true, "Send this queued message now"],
    ["stopAndSend", "Stop and send", true, null],
    // The first row is a clear alias, so it runs as its own turn.
    ["stopAndRun", "Stop and run", true, null],
    ["startAndSend", "Start and send", false, null],
  ])("the %s action renders %s", (action, text, viaSendNow, ariaLabel) => {
    const sent: string[] = [];
    const first = action === "stopAndRun" ? "/clear" : "go";
    const btn = renderWithProfile("claude", [mk("a", first)], {
      reason: { label: "Queued", reason: "why", action: action === "stopAndRun" ? "stopAndSend" : action },
      onSendNow: (p) => sent.push(`send:${p.text}`),
      onStartAndSend: () => sent.push("start"),
    }).getByTestId("queued-send-now");
    expect(btn.getAttribute("aria-label") ?? btn.textContent).toBe(ariaLabel ?? text);
    fireEvent.click(btn);
    expect(sent).toEqual([viaSendNow ? `send:${first}` : "start"]);
  });

  it("keeps a waiting row's action focusable, says why, and does nothing on click", () => {
    const sent: QueuedPrompt[] = [];
    const reason: QueuedReason = {
      label: "Queued",
      reason: "sends after compacting",
      action: "disabled",
      disabledReason: "Waiting: this message sends after compacting",
    };
    const btn = renderWithProfile("claude", [mk("a", "go")], { reason, onSendNow: (p) => sent.push(p) }).getByTestId(
      "queued-send-now",
    );
    expect(btn.getAttribute("aria-disabled")).toBe("true");
    expect(btn.getAttribute("aria-label")).toBe("Waiting: this message sends after compacting");
    expect((btn as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(btn);
    expect(sent).toEqual([]);
  });

  it.each([
    [true, false],
    [false, true],
  ])("Send now with canSendNow=%s is disabled=%s", (canSendNow, disabled) => {
    const btn = renderWithProfile("claude", [mk("a", "go")], { canSendNow }).getByTestId("queued-send-now");
    expect((btn as HTMLButtonElement).disabled).toBe(disabled);
  });

  it("hides the row action when the row can only be dropped", () => {
    const reason: QueuedReason = { label: "Not sent", reason: "x", action: "dropOnly" };
    expect(renderWithProfile("claude", [mk("a", "go")], { reason }).queryByTestId("queued-send-now")).toBeNull();
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

    fireEvent.click(getByRole("button", { name: "Show full queued message" }));
    expect(getByTitle("Click to edit").className).not.toContain("line-clamp-3");
    const box = getByTitle("Click to edit").parentElement as HTMLElement;
    expect(box.className).toContain("max-h-48");
    expect(box.className).toContain("overflow-y-auto");
    const collapseToggle = getByRole("button", { name: "Collapse queued message" });
    expect(collapseToggle.textContent).toBe("Show less");
    expect(box.contains(collapseToggle)).toBe(false);
  });
});
