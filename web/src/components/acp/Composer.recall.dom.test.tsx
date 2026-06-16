// @vitest-environment jsdom
//
// RTL mount of the real <Composer> exercising the ArrowUp/ArrowDown queue
// recall handlers, the "Editing queued message" banner, Esc-restore, and
// the edit-in-place submit path. The mocked + live Playwright specs drive
// the same flow, but the v8(vitest)+istanbul merge keeps v8 line hits more
// reliably, so this jsdom mount is what lifts the component handlers in the
// merged coverage report. Heavy/IO hooks are stubbed; the assistant-ui
// runtime is provided via useExternalStoreRuntime like Composer.escape.test.

import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";

import { Composer } from "./Composer";
import type { QueuedPrompt } from "../../lib/acpTypes";

vi.mock("./useFilesIndex", () => ({
  useFilesIndex: () => ({ files: [] }),
  fuzzyFilter: <T,>(items: T[]) => items,
}));
vi.mock("./SessionConfigControls", () => ({ SessionConfigControls: () => null }));
vi.mock("./SwitchAgentModal", () => ({ SwitchAgentModal: () => null }));
vi.mock("../../hooks/useMobileKeyboard", () => ({ useMobileKeyboard: () => ({ keyboardOpen: false }) }));
vi.mock("../../hooks/useFocusTerminalTarget", () => ({ useFocusTerminalTarget: () => {} }));
vi.mock("../../lib/agentProfileContext", () => ({
  useAgentProfile: () => ({ clearAliases: [], capabilities: { legacyModeFallback: false } }),
}));
vi.mock("../../lib/acpDrafts", () => ({ getDraft: () => "", setDraft: () => {} }));
vi.mock("./useDictationBurstGuard", () => ({
  useDictationBurstGuard: () => ({
    observeInputType: () => {},
    shouldSuppressUpstream: () => false,
    flushOnBlur: () => {},
  }),
}));

const QUEUE: QueuedPrompt[] = [
  { id: "q-a", text: "first queued", queuedAt: "2026-01-01T00:00:00Z" },
  { id: "q-b", text: "second queued", queuedAt: "2026-01-01T00:00:01Z" },
];

function Harness({
  editQueuedPrompt = () => {},
  enqueuePrompt = () => {},
  queue = QUEUE,
}: {
  editQueuedPrompt?: (id: string, text: string) => void;
  enqueuePrompt?: (text: string) => void;
  queue?: QueuedPrompt[];
}) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: true,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="s-recall"
        currentAgent={"claude" as never}
        availableModes={[] as never}
        currentModeId={null as never}
        legacyMode={null as never}
        configOptions={[] as never}
        pendingConfigOption={null as never}
        setConfigOption={() => {}}
        sessionUsage={null as never}
        availableCommands={[] as never}
        connected={true}
        turnActive={true}
        queuedCount={queue.length}
        enqueuePrompt={enqueuePrompt}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
        primerPrefill={null}
        queuedPrompts={queue}
        editQueuedPrompt={editQueuedPrompt}
      />
    </AssistantRuntimeProvider>
  );
}

function getComposer(): HTMLTextAreaElement {
  return screen.getByRole("textbox") as HTMLTextAreaElement;
}

let rafSpy: ReturnType<typeof vi.spyOn> | undefined;

beforeEach(() => {
  // assistant-ui's ComposerPrimitive.Input calls useMediaQuery, which jsdom
  // does not implement; stub a non-matching media query.
  if (!window.matchMedia) {
    window.matchMedia = ((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    })) as typeof window.matchMedia;
  }
  // Run requestAnimationFrame callbacks synchronously so loadRecallText's
  // focus/caret/resize body executes within the test.
  rafSpy = vi.spyOn(globalThis, "requestAnimationFrame").mockImplementation((cb: FrameRequestCallback) => {
    cb(0);
    return 0;
  });
});

afterEach(() => {
  rafSpy?.mockRestore();
  cleanup();
});

describe("<Composer> queue recall (#2147)", () => {
  it("ArrowUp recalls, banner shows, ArrowDown cycles, Esc restores", async () => {
    render(<Harness editQueuedPrompt={() => {}} />);
    const ta = getComposer();
    expect(ta.value).toBe("");

    // ArrowUp at the empty/origin composer enters recall on the newest.
    ta.focus();
    ta.setSelectionRange(0, 0);
    fireEvent.keyDown(ta, { key: "ArrowUp" });
    await waitFor(() => expect(getComposer().value).toBe("second queued"));
    expect(screen.queryByText(/Editing queued message 1 of 2/)).not.toBeNull();

    // ArrowUp again walks to the older entry.
    fireEvent.keyDown(getComposer(), { key: "ArrowUp" });
    await waitFor(() => expect(getComposer().value).toBe("first queued"));
    expect(screen.queryByText(/Editing queued message 2 of 2/)).not.toBeNull();

    // ArrowUp at the oldest is a no-op (no wrap).
    fireEvent.keyDown(getComposer(), { key: "ArrowUp" });
    await waitFor(() => expect(getComposer().value).toBe("first queued"));

    // ArrowDown walks back toward newer.
    fireEvent.keyDown(getComposer(), { key: "ArrowDown" });
    await waitFor(() => expect(getComposer().value).toBe("second queued"));

    // Esc restores the stashed (empty) draft and clears the banner.
    fireEvent.keyDown(getComposer(), { key: "Escape" });
    await waitFor(() => expect(getComposer().value).toBe(""));
    expect(screen.queryByText(/Editing queued message/)).toBeNull();
  });

  it("ArrowDown past the newest restores the stashed draft", async () => {
    render(<Harness editQueuedPrompt={() => {}} />);
    const ta = getComposer();
    ta.focus();
    ta.setSelectionRange(0, 0);
    fireEvent.keyDown(ta, { key: "ArrowUp" });
    await waitFor(() => expect(getComposer().value).toBe("second queued"));
    // Already at the newest: ArrowDown restores the draft and exits.
    fireEvent.keyDown(getComposer(), { key: "ArrowDown" });
    await waitFor(() => expect(getComposer().value).toBe(""));
    expect(screen.queryByText(/Editing queued message/)).toBeNull();
  });

  it("editing a recalled prompt and pressing Enter edits it in place", async () => {
    const editQueuedPrompt = vi.fn();
    render(<Harness editQueuedPrompt={editQueuedPrompt} />);
    const ta = getComposer();
    ta.focus();
    ta.setSelectionRange(0, 0);
    fireEvent.keyDown(ta, { key: "ArrowUp" });
    await waitFor(() => expect(getComposer().value).toBe("second queued"));

    fireEvent.change(getComposer(), { target: { value: "second queued edited" } });
    fireEvent.keyDown(getComposer(), { key: "Enter" });

    await waitFor(() => expect(editQueuedPrompt).toHaveBeenCalledWith("q-b", "second queued edited"));
  });

  it("plain Enter when not browsing sends through enqueue", async () => {
    const enqueuePrompt = vi.fn();
    render(<Harness enqueuePrompt={enqueuePrompt} />);
    const ta = getComposer();
    fireEvent.change(ta, { target: { value: "brand new" } });
    fireEvent.keyDown(ta, { key: "Enter" });
    await waitFor(() => expect(enqueuePrompt).toHaveBeenCalledWith("brand new", undefined));
  });

  it("draining the browsed entry mid-browse exits recall, keeping the text", async () => {
    const { rerender } = render(<Harness />);
    const ta = getComposer();
    ta.focus();
    ta.setSelectionRange(0, 0);
    fireEvent.keyDown(ta, { key: "ArrowUp" });
    await waitFor(() => expect(getComposer().value).toBe("second queued"));

    // The browsed entry (q-b) drains away; only q-a remains. The next
    // ArrowUp finds the cursor id gone and exits, leaving the text put.
    rerender(<Harness queue={[QUEUE[0]!]} />);
    fireEvent.keyDown(getComposer(), { key: "ArrowUp" });
    await waitFor(() => expect(screen.queryByText(/Editing queued message/)).toBeNull());
    expect(getComposer().value).toBe("second queued");
  });
});
