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

const IDLE: QueuedReason = { label: "Queued", reason: null, action: "send", soloAction: "send" };

function renderWithProfile(
  toolKey: string,
  queued: QueuedPrompt[],
  opts: {
    onSendNow?: (prompt: QueuedPrompt) => void;
    canSendNow?: boolean;
    onStartAndSend?: () => void;
    onStopTurn?: () => void;
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
        onStopTurn={opts.onStopTurn}
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
      { label: "Queued", reason: "sends when Codex finishes", action: "stopAndSend", soloAction: "stopAndRun" },
      "Queued (1)",
      "· sends when Codex finishes",
    ],
    ["an idle queue has no reason", IDLE, "Queued (1)", null],
    // Offline with a row the Daemon never got: nothing has been sent yet.
    [
      "an offline pending row",
      { label: "Not sent", reason: "sends when this dashboard reconnects", action: "dropOnly", soloAction: "dropOnly" },
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

  // A message row and a solo command row side by side, as the Daemon would queue them.
  it.each<[string, QueuedReason["action"], QueuedReason["soloAction"], [string, string], [string, string]]>([
    [
      "idle",
      "send",
      "send",
      ["Send this queued message now", "send:go"],
      ["Send this queued message now", "send:/compact"],
    ],
    // A steerable turn takes the message now, but the command needs the turn stopped first.
    ["a steerable turn", "send", "stopAndRun", ["Send this queued message now", "send:go"], ["Stop and run", "stop"]],
    [
      "a turn that can't be steered",
      "stopAndSend",
      "stopAndRun",
      ["Stop and send", "send:go"],
      ["Stop and run", "stop"],
    ],
    ["a stopped Worker", "startAndSend", "startAndSend", ["Start and send", "start"], ["Start and send", "start"]],
  ])("during %s, rows offer %s and %s", (_label, action, soloAction, plain, solo) => {
    const calls: string[] = [];
    const buttons = renderWithProfile("claude", [mk("a", "go"), mk("b", "/compact")], {
      reason: { label: "Queued", reason: "why", action, soloAction },
      onSendNow: (p) => calls.push(`send:${p.text}`),
      onStartAndSend: () => calls.push("start"),
      onStopTurn: () => calls.push("stop"),
    }).getAllByTestId("queued-send-now");
    for (const [btn, [name, call]] of [
      [buttons[0], plain],
      [buttons[1], solo],
    ] as const) {
      expect(btn.getAttribute("aria-label") ?? btn.textContent).toBe(name);
      expect(btn.getAttribute("aria-disabled")).toBeNull();
      // Text labels read at a contrast-validated token; the unread hue stays on the row's outline.
      if (!btn.getAttribute("aria-label")) {
        expect(btn.className).toContain("text-text-primary");
        expect(btn.className).not.toContain("text-status-unread");
      }
      calls.length = 0;
      fireEvent.click(btn);
      expect(calls).toEqual([call]);
    }
  });

  it.each<[string, Partial<QueuedReason>, boolean, string | null]>([
    [
      "compacting",
      {
        reason: "sends after compacting",
        action: "disabled",
        disabledReason: "Waiting: this message sends after compacting",
      },
      true,
      "Waiting: this message sends after compacting",
    ],
    // A new Session can't send yet: the strip's reason explains the wait instead of a generic hint.
    [
      "a starting Session",
      { reason: "sends when Claude Code is ready" },
      false,
      "Waiting: this message sends when Claude Code is ready",
    ],
    ["no live connection", {}, false, "Send now needs this dashboard connected and the Agent running"],
    ["a free Agent", {}, true, null],
  ])("Send now during %s", (_label, over, canSendNow, waiting) => {
    const sent: QueuedPrompt[] = [];
    const reason = { ...IDLE, ...over, soloAction: over.action ?? IDLE.soloAction };
    const btn = renderWithProfile("claude", [mk("a", "go")], {
      reason,
      canSendNow,
      onSendNow: (p) => sent.push(p),
    }).getByTestId("queued-send-now");
    // `aria-disabled` keeps the button focusable, so its tooltip and name can say why.
    expect((btn as HTMLButtonElement).disabled).toBe(false);
    expect(btn.getAttribute("aria-disabled")).toBe(waiting ? "true" : null);
    expect(btn.getAttribute("aria-label")).toBe(waiting ?? "Send this queued message now");
    fireEvent.click(btn);
    expect(sent).toHaveLength(waiting ? 0 : 1);
  });

  it("hides the row action when the row can only be dropped", () => {
    const reason: QueuedReason = { label: "Not sent", reason: "x", action: "dropOnly", soloAction: "dropOnly" };
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
