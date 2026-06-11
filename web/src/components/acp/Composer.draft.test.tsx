// @vitest-environment jsdom
//
// User stories (ported from the live Playwright acp-stories suite):
//
// 1. Composer draft persists across a full page reload. The Composer
//    mirrors the textarea into localStorage at `acp:draft:<sessionId>`
//    with a 250ms debounce, and the mount effect seeds the composer
//    from the same key, so the user does not lose an in-progress
//    prompt when the page reloads (a remount with a fresh runtime).
//
// 2. Composer draft persists across a session switch. Drafts are
//    keyed per session id, so typing into session A, navigating to
//    session B (the StructuredView for A unmounts), and returning to
//    A re-seeds A's draft while B starts empty.
//
// Both stories reduce to the same component contract: the draft
// effect in Composer.tsx writes through lib/acpDrafts on a debounce
// (plus an unmount flush) and re-seeds the textarea on mount. The
// storage module itself is covered by lib/acpDrafts.test.ts; this
// file covers the Composer wiring.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";

import { Composer } from "./Composer";

function HarnessComposer({ sessionId }: { sessionId: string }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId={sessionId}
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
        turnActive={false}
        queuedCount={0}
        enqueuePrompt={() => {}}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
      />
    </AssistantRuntimeProvider>
  );
}

function mountComposer(sessionId: string) {
  const utils = render(<HarnessComposer sessionId={sessionId} />);
  const textarea = utils.container.querySelector("textarea");
  if (!textarea) throw new Error("composer textarea not rendered");
  return { ...utils, textarea };
}

// assistant-ui's store flushes runtime-driven text updates (the draft
// seed path uses composerRuntime.setText) on a scheduled task, not
// synchronously within the mount effect, so the textarea only reflects
// a seeded draft after the timer queue drains.
async function flushComposer() {
  await act(async () => {
    vi.advanceTimersByTime(50);
  });
}

beforeEach(() => {
  window.localStorage.clear();
  // jsdom has no matchMedia; both assistant-ui's ComposerPrimitive.Input
  // and the Composer's touch-input detection probe it. A never-matching
  // stub yields the desktop code path.
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
  // useFilesIndex fetches the @-mention file list on mount; an empty
  // index keeps the harness self-contained.
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ files: [] }),
    }),
  );
  vi.useFakeTimers();
});

afterEach(() => {
  // Unmount before restoring real timers so the unmount flush in the
  // draft effect does not race the next test's storage assertions.
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("Composer per-session draft persistence", () => {
  it("mirrors typed text into acp:draft:<sessionId> after the 250ms debounce", () => {
    const { textarea } = mountComposer("sess-reload");

    fireEvent.change(textarea, { target: { value: "unsent draft text" } });
    // Not yet flushed: the write is debounced.
    expect(window.localStorage.getItem("acp:draft:sess-reload")).toBeNull();

    act(() => {
      vi.advanceTimersByTime(250);
    });
    expect(window.localStorage.getItem("acp:draft:sess-reload")).toBe("unsent draft text");
  });

  it("re-seeds the textarea from the persisted draft on a fresh mount (reload story)", async () => {
    window.localStorage.setItem("acp:draft:sess-reload", "unsent draft text");

    const { textarea } = mountComposer("sess-reload");
    await flushComposer();
    expect(textarea.value).toBe("unsent draft text");
  });

  it("keeps drafts keyed per session across a switch away and back", async () => {
    const first = mountComposer("sess-a");
    fireEvent.change(first.textarea, { target: { value: "draft for A" } });
    act(() => {
      vi.advanceTimersByTime(250);
    });
    // Switching to another session unmounts the StructuredView (and
    // this Composer) for A.
    first.unmount();

    const second = mountComposer("sess-b");
    await flushComposer();
    // B must not inherit A's draft.
    expect(second.textarea.value).toBe("");
    second.unmount();

    const back = mountComposer("sess-a");
    await flushComposer();
    expect(back.textarea.value).toBe("draft for A");
  });

  it("flushes the pending debounced write on unmount so a fast switch loses nothing", () => {
    const { textarea, unmount } = mountComposer("sess-a");
    fireEvent.change(textarea, { target: { value: "typed then switched" } });
    // Unmount before the 250ms debounce fires; the effect cleanup
    // flush must still persist the text.
    unmount();
    expect(window.localStorage.getItem("acp:draft:sess-a")).toBe("typed then switched");
  });
});
