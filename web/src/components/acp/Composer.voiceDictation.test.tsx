// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";

import { Composer } from "./Composer";
import type { SpeechRecognitionLike } from "./useVoiceDictation";

class MockSpeechRecognition implements SpeechRecognitionLike {
  static instances: MockSpeechRecognition[] = [];

  continuous = false;
  interimResults = false;
  lang = "";
  onresult: SpeechRecognitionLike["onresult"] = null;
  onerror: SpeechRecognitionLike["onerror"] = null;
  onend: SpeechRecognitionLike["onend"] = null;
  start = vi.fn();
  stop = vi.fn(() => {
    this.onend?.();
  });
  abort = vi.fn();

  constructor() {
    MockSpeechRecognition.instances.push(this);
  }

  emitFinal(transcript: string) {
    const list = [{ isFinal: true, 0: { transcript } }];
    this.onresult?.({
      resultIndex: 0,
      results: Object.assign(list, { length: list.length }),
    });
  }

  emitEnd() {
    this.onend?.();
  }
}

function Harness({ enqueuePrompt = vi.fn() }: { enqueuePrompt?: (text: string) => void }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="sess-voice"
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
        enqueuePrompt={enqueuePrompt}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
      />
    </AssistantRuntimeProvider>
  );
}

beforeEach(() => {
  MockSpeechRecognition.instances = [];
  window.localStorage.clear();
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: query === "(pointer: coarse)",
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    })),
  );
  vi.stubGlobal("webkitSpeechRecognition", MockSpeechRecognition);
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

describe("Composer voice dictation", () => {
  it("shows a mobile mic button in the action slot when the draft is empty", () => {
    render(<Harness />);
    expect(screen.getByRole("button", { name: "Start voice dictation" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Send message" })).toBeNull();
  });

  it("replaces cumulative transcript updates in the draft without sending", () => {
    const enqueuePrompt = vi.fn();
    const { container } = render(<Harness enqueuePrompt={enqueuePrompt} />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    fireEvent.click(screen.getByRole("button", { name: "Start voice dictation" }));
    MockSpeechRecognition.instances[0]!.emitFinal("review");
    MockSpeechRecognition.instances[0]!.emitFinal("review the diff");

    expect(textarea.value).toBe("review the diff");
    fireEvent.click(screen.getByRole("button", { name: "Stop voice dictation" }));
    expect(screen.getByRole("button", { name: "Send message" })).toBeTruthy();
    expect(enqueuePrompt).not.toHaveBeenCalled();
  });

  it("keeps dictated text cumulative after browser recognition auto-restarts", async () => {
    const { container } = render(<Harness />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    fireEvent.click(screen.getByRole("button", { name: "Start voice dictation" }));
    MockSpeechRecognition.instances[0]!.emitFinal("review the");
    MockSpeechRecognition.instances[0]!.emitEnd();

    await waitFor(() => expect(MockSpeechRecognition.instances).toHaveLength(2));
    MockSpeechRecognition.instances[1]!.emitFinal("diff please");

    expect(textarea.value).toBe("review the diff please");
  });

  it("swaps to Send once the user starts typing", () => {
    const { container } = render(<Harness />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    fireEvent.change(textarea, { target: { value: "please" } });

    expect(screen.queryByRole("button", { name: "Start voice dictation" })).toBeNull();
    expect(screen.getByRole("button", { name: "Send message" })).toBeTruthy();
  });
});
