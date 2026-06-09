// @vitest-environment jsdom
//
// User story (ported from the live Playwright acp-stories suite):
// clicking @ or / on the composer toolbar inserts the trigger
// character into the textarea.
//
// ToolbarButton aria-labels are "Add file context (@)" and
// "Slash command (/)"; both call insertAtCaret on the textarea ref.
// The insertAtCaret helper itself (InputEvent shape, mid-word space
// padding) is covered by Composer.insert.test.ts; this file covers
// the button wiring through a mounted Composer.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";

import { Composer } from "./Composer";

function Harness() {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="sess-toolbar"
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
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("Composer toolbar trigger buttons", () => {
  it("inserts @ into the textarea via the 'Add file context (@)' button", () => {
    const { container } = render(<Harness />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    fireEvent.click(screen.getByRole("button", { name: "Add file context (@)" }));
    // The @ popover machinery may pad the trigger with a space when the
    // caret sits mid-word, so assert containment rather than equality.
    expect(textarea.value).toContain("@");
  });

  it("inserts / after @ via the 'Slash command (/)' button", () => {
    const { container } = render(<Harness />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    fireEvent.click(screen.getByRole("button", { name: "Add file context (@)" }));
    fireEvent.click(screen.getByRole("button", { name: "Slash command (/)" }));
    expect(textarea.value).toMatch(/@.*\//s);
  });
});
