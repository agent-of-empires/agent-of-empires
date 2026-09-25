// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import {
  AssistantRuntimeProvider,
  ComposerPrimitive,
  useExternalStoreRuntime,
  type ThreadMessageLike,
} from "@assistant-ui/react";

import type { PluginUiEntry } from "../../lib/api";
import type { PromptAttachmentInput, QueuedPrompt } from "../../lib/acpTypes";
import { buildSkillIndex, type SkillIndex } from "../../lib/skillProvenance";
import { Composer } from "./Composer";

const { skillIndexRef, entriesRef } = vi.hoisted(() => ({
  skillIndexRef: { current: { labelsByKey: new Map<string, Set<string>>() } as SkillIndex },
  entriesRef: { current: [] as PluginUiEntry[] },
}));
vi.mock("../../hooks/useSkillIndex", () => ({ useSkillIndex: () => skillIndexRef.current }));
vi.mock("../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => entriesRef.current,
  usePluginUiPoke: () => vi.fn(),
  usePluginUiRefreshing: () => false,
  usePluginUiRevision: () => 0,
}));

type ComposerProps = React.ComponentProps<typeof Composer>;

function Harness({
  isRunning = false,
  onCancel,
  ...overrides
}: Partial<ComposerProps> & { isRunning?: boolean; onCancel?: () => void }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning,
    convertMessage: (m) => m,
    onNew: async () => {},
    onCancel,
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="sess"
        currentAgent="claude"
        availableModes={[]}
        currentModeId={null}
        legacyMode="Default"
        configOptions={[]}
        pendingConfigOption={null}
        setConfigOption={() => {}}
        sessionUsage={null}
        availableCommands={[]}
        connected
        turnActive={isRunning}
        enqueuePrompt={() => {}}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
        queuedPrompts={[]}
        editQueuedPrompt={() => {}}
        {...overrides}
      />
    </AssistantRuntimeProvider>
  );
}

function mount(props: Parameters<typeof Harness>[0] = {}) {
  const utils = render(<Harness {...props} />);
  const textarea = () => utils.container.querySelector("textarea")!;
  return { ...utils, textarea };
}

beforeEach(() => {
  window.localStorage.clear();
  entriesRef.current = [];
  skillIndexRef.current = { labelsByKey: new Map() };
  // jsdom has no matchMedia; never matching selects the desktop path.
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    })),
  );
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, json: async () => ({ files: [] }) }));
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

/** assistant-ui applies composer writes on a scheduled task. */
async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

describe("toolbar and send", () => {
  it("enables Send only for non-whitespace text", () => {
    const { textarea } = mount();
    const send = screen.getByRole("button", { name: "Send message" }) as HTMLButtonElement;
    expect(send.disabled).toBe(true);
    fireEvent.change(textarea(), { target: { value: "hello" } });
    expect(send.disabled).toBe(false);
    fireEvent.change(textarea(), { target: { value: "   " } });
    expect(send.disabled).toBe(true);
  });

  it.each([
    [{ amount: 0.42, currency: "USD" }, 120_000, "60%"],
    [null, 50_000, "25%"],
  ])("exposes usage and optional spend in the hint and tooltip (cost %o)", (cost, used, pct) => {
    mount({ sessionUsage: { used, size: 200_000, cost } });
    const spend = new Intl.NumberFormat(undefined, { style: "currency", currency: "USD" }).format(0.42);
    const hint = screen.getByLabelText(/Context window:/);
    expect(hint.textContent).toContain(pct);
    expect(hint.textContent?.includes(spend)).toBe(cost !== null);
    fireEvent.mouseEnter(hint.parentElement!);
    const tip = screen.getByRole("tooltip").textContent ?? "";
    expect(tip).toContain(used.toLocaleString());
    expect(tip).toContain(pct);
    expect(tip.includes(spend)).toBe(cost !== null);
  });

  it("stages pasted image files and leaves text paste alone", async () => {
    const setPendingAttachments = vi.fn();
    const { textarea } = mount({
      promptCapabilities: { image: true, audio: false, embeddedContext: false },
      setPendingAttachments,
    });
    const text = fireEvent.paste(textarea(), {
      clipboardData: { items: [], getData: (type: string) => (type === "text/plain" ? "plain text" : "") },
    });
    expect(text).toBe(true);
    expect(setPendingAttachments).not.toHaveBeenCalled();

    const image = new File([new Uint8Array([1, 2, 3])], "shot.png", { type: "image/png" });
    expect(fireEvent.paste(textarea(), { clipboardData: { items: [{ kind: "file", getAsFile: () => image }] } })).toBe(
      false,
    );
    await waitFor(() => expect(setPendingAttachments).toHaveBeenCalled());
    const update = setPendingAttachments.mock.calls[0]![0] as (
      prev: PromptAttachmentInput[],
    ) => PromptAttachmentInput[];
    expect(update([])).toEqual([{ kind: "image", mimeType: "image/png", name: "shot.png", dataB64: "AQID" }]);
  });

  it("applies each plugin draft operation id once", async () => {
    const entry = (op: Record<string, unknown>): PluginUiEntry => ({
      plugin_id: "acme.voice",
      slot: "composer-action",
      id: "dictate",
      session_id: "sess",
      payload: { label: "Voice", method: "voice.start", draft_operation: op },
    });
    entriesRef.current = [entry({ kind: "insert-text", id: "op-1", text: "hello" })];
    const { textarea, rerender } = mount();
    await waitFor(() => expect(textarea().value).toBe("hello"));
    rerender(<Harness />);
    await waitFor(() => expect(textarea().value).toBe("hello"));
    entriesRef.current = [entry({ kind: "insert-text", id: "op-2", text: " world" })];
    rerender(<Harness />);
    await waitFor(() => expect(textarea().value).toBe("hello world"));
  });
});

describe("Escape", () => {
  // The control proves Escape reaches assistant-ui's cancel binding in this setup.
  it("does not cancel the running turn, although the primitive's default would", async () => {
    const pressEscape = (el: HTMLElement) => {
      el.focus();
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    };
    const composerCancel = vi.fn();
    const { textarea } = mount({ isRunning: true, onCancel: composerCancel });
    pressEscape(textarea());
    await Promise.resolve();
    expect(composerCancel).not.toHaveBeenCalled();
    cleanup();

    const defaultCancel = vi.fn();
    function Bare() {
      const runtime = useExternalStoreRuntime<ThreadMessageLike>({
        messages: [],
        isRunning: true,
        convertMessage: (m) => m,
        onNew: async () => {},
        onCancel: defaultCancel,
      });
      return (
        <AssistantRuntimeProvider runtime={runtime}>
          <ComposerPrimitive.Root>
            <ComposerPrimitive.Input data-testid="bare-input" />
          </ComposerPrimitive.Root>
        </AssistantRuntimeProvider>
      );
    }
    render(<Bare />);
    pressEscape(screen.getByTestId("bare-input"));
    await Promise.resolve();
    expect(defaultCancel).toHaveBeenCalledTimes(1);
  });
});

describe("slash command popover", () => {
  const COMMANDS = [
    { name: "address-pr-comments", description: "Address PR comments", accepts_input: false },
    { name: "help", description: "Show help", accepts_input: false },
  ];
  const option = (text: RegExp) => screen.queryAllByRole("option").filter((el) => text.test(el.textContent ?? ""));

  /** Type with the caret positioned before the input event, as a user would. */
  async function typeAt(ta: HTMLTextAreaElement, value: string, caret: number) {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(ta, value);
    ta.setSelectionRange(caret, caret);
    fireEvent.input(ta);
    await flush();
  }

  // Items are clicked: jsdom does not route synthetic keydowns to the popover plugin.
  async function pick(label: RegExp) {
    fireEvent.click(option(label)[0]!);
    await flush();
  }

  it("closes the popover on accept so the next Enter is not swallowed", async () => {
    const { textarea } = mount({ availableCommands: COMMANDS });
    await typeAt(textarea(), "/addr", 5);
    expect(option(/address-pr-comments/)).toHaveLength(1);

    // Premise: jsdom must not resync the caret the way browsers usually do,
    // or this stops reproducing the stale-cursor corruption.
    let selectionChanges = 0;
    const count = () => selectionChanges++;
    document.addEventListener("selectionchange", count);
    await pick(/address-pr-comments/);
    document.removeEventListener("selectionchange", count);

    expect(textarea().value).toBe("/address-pr-comments ");
    expect(selectionChanges, "jsdom now resyncs the caret; this test no longer reproduces #3418").toBe(0);
    expect(option(/address-pr-comments/)).toHaveLength(0);
  });

  it("badges a skill-backed command and leaves a plain one unbadged", async () => {
    skillIndexRef.current = buildSkillIndex({
      roots: [
        { id: "claude-user", label: "Claude", relativePath: ".claude/skills", consumers: ["claude"], legacy: false },
      ],
      skills: [
        {
          directory: "aoe-review",
          name: "aoe-review",
          description: "",
          provenance: { kind: "external", root: "claude-user" },
          provenanceLabel: "external:claude-user",
          writable: false,
        },
      ],
    });
    const { textarea } = mount({
      availableCommands: [
        { name: "aoe-review", description: "Run the review skill", accepts_input: false },
        COMMANDS[1]!,
      ],
    });
    fireEvent.change(textarea(), { target: { value: "/" } });
    await waitFor(() => expect(option(/\/aoe-review/)).toHaveLength(1));
    expect(option(/\/aoe-review/)[0]!.textContent).toContain("Claude");
    expect(option(/\/help/)[0]!.textContent).not.toContain("Claude");
  });
});

describe("queue recall", () => {
  const QUEUE: QueuedPrompt[] = [
    { id: "q-a", text: "first queued", queuedAt: "2026-01-01T00:00:00Z" },
    { id: "q-b", text: "second queued", queuedAt: "2026-01-01T00:00:01Z" },
  ];

  beforeEach(() => {
    // Run rAF synchronously so the focus/caret/resize after loading text executes.
    vi.spyOn(globalThis, "requestAnimationFrame").mockImplementation((cb: FrameRequestCallback) => {
      cb(0);
      return 0;
    });
  });

  function mountRecall(props: Parameters<typeof Harness>[0] = {}) {
    const utils = mount({ isRunning: true, queuedPrompts: QUEUE, ...props });
    const key = async (key: string, expected: string) => {
      fireEvent.keyDown(utils.textarea(), { key });
      await waitFor(() => expect(utils.textarea().value).toBe(expected));
    };
    const enter = async () => {
      utils.textarea().focus();
      utils.textarea().setSelectionRange(0, 0);
      await key("ArrowUp", "second queued");
    };
    const banner = () => screen.queryByText(/Editing queued message/)?.textContent ?? null;
    return { ...utils, key, enter, banner };
  }

  it("walks older and newer, stops at the oldest, and Esc restores the draft", async () => {
    const { key, enter, banner } = mountRecall();
    await enter();
    expect(banner()).toContain("1 of 2");
    await key("ArrowUp", "first queued");
    expect(banner()).toContain("2 of 2");
    await key("ArrowUp", "first queued");
    await key("ArrowDown", "second queued");
    await key("Escape", "");
    expect(banner()).toBeNull();
  });

  it("edits a recalled prompt in place on Enter", async () => {
    const editQueuedPrompt = vi.fn();
    const { textarea, enter } = mountRecall({ editQueuedPrompt });
    await enter();
    fireEvent.change(textarea(), { target: { value: "second queued edited" } });
    fireEvent.keyDown(textarea(), { key: "Enter" });
    await waitFor(() => expect(editQueuedPrompt).toHaveBeenCalledWith("q-b", "second queued edited"));
  });

  it("sends through enqueue on plain mid-turn Enter", async () => {
    const enqueuePrompt = vi.fn();
    const { textarea } = mountRecall({ enqueuePrompt });
    fireEvent.change(textarea(), { target: { value: "brand new" } });
    fireEvent.keyDown(textarea(), { key: "Enter" });
    await waitFor(() => expect(enqueuePrompt).toHaveBeenCalledWith("brand new", undefined));
  });

  it("exits recall, keeping the text, when the browsed entry drains", async () => {
    const { textarea, enter, banner, rerender } = mountRecall();
    await enter();
    rerender(<Harness isRunning queuedPrompts={[QUEUE[0]!]} />);
    fireEvent.keyDown(textarea(), { key: "ArrowUp" });
    await waitFor(() => expect(banner()).toBeNull());
    expect(textarea().value).toBe("second queued");
  });
});

describe("draft persistence", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  const stored = (id: string) => window.localStorage.getItem(`acp:draft:${id}`);
  const advance = (ms: number) =>
    act(() => {
      vi.advanceTimersByTime(ms);
    });
  // Seeded text arrives via setText, which the store applies on a scheduled task.
  async function mountSession(sessionId: string) {
    const utils = mount({ sessionId });
    await act(async () => {
      vi.advanceTimersByTime(50);
    });
    return utils;
  }

  it("writes the draft after the 250ms debounce and re-seeds it on remount", async () => {
    const first = await mountSession("sess-reload");
    fireEvent.change(first.textarea(), { target: { value: "unsent draft text" } });
    expect(stored("sess-reload")).toBeNull();
    advance(250);
    expect(stored("sess-reload")).toBe("unsent draft text");
    first.unmount();
    expect((await mountSession("sess-reload")).textarea().value).toBe("unsent draft text");
  });

  it("clears the draft synchronously on send so a racing remount cannot restore it", async () => {
    const first = await mountSession("sess-send");
    fireEvent.change(first.textarea(), { target: { value: "sent text" } });
    advance(250);
    expect(stored("sess-send")).toBe("sent text");
    act(() => {
      fireEvent.click(first.getByLabelText("Send message"));
    });
    expect(stored("sess-send")).toBeNull();
    first.unmount();
    expect((await mountSession("sess-send")).textarea().value).toBe("");
  });

  it("flushes a pending write on unmount", async () => {
    const { textarea, unmount } = await mountSession("sess-a");
    fireEvent.change(textarea(), { target: { value: "typed then switched" } });
    unmount();
    expect(stored("sess-a")).toBe("typed then switched");
  });
});
