// @vitest-environment jsdom
//
// Clipboard image paste in the structured-view composer (#965). The live
// backend attachment round trip is covered by tests/live/acp-attachment.spec.ts;
// this file pins the browser paste event boundary that stages clipboard files.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import type { Dispatch, SetStateAction } from "react";

import { Composer } from "./Composer";
import type { PromptAttachmentInput } from "../../lib/acpTypes";

vi.mock("./useFilesIndex", () => ({
  useFilesIndex: () => ({ files: [] }),
  fuzzyFilter: <T,>(items: T[]) => items,
}));

function stubMatchMedia() {
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
}

function Harness({
  setPendingAttachments,
}: {
  setPendingAttachments: Dispatch<SetStateAction<PromptAttachmentInput[]>>;
}) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="sess-paste"
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
        queuedPrompts={[]}
        enqueuePrompt={() => {}}
        editQueuedPrompt={() => {}}
        promptCapabilities={{ image: true, audio: false, embeddedContext: false }}
        pendingAttachments={[]}
        setPendingAttachments={setPendingAttachments}
      />
    </AssistantRuntimeProvider>
  );
}

function mountComposer(setPendingAttachments = vi.fn()) {
  const utils = render(<Harness setPendingAttachments={setPendingAttachments} />);
  const textarea = utils.container.querySelector("textarea");
  if (!textarea) throw new Error("composer textarea not rendered");
  return { ...utils, textarea, setPendingAttachments };
}

beforeEach(() => {
  window.localStorage.clear();
  stubMatchMedia();
  vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("fetch not expected in paste flow")));
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("Composer clipboard paste attachments", () => {
  it("stages pasted image file items when the agent accepts images", async () => {
    const setPendingAttachments = vi.fn();
    const { textarea } = mountComposer(setPendingAttachments);
    const image = new File([new Uint8Array([1, 2, 3])], "shot.png", { type: "image/png" });

    expect(
      fireEvent.paste(textarea, {
        clipboardData: {
          items: [{ kind: "file", getAsFile: () => image }],
        },
      }),
    ).toBe(false);

    await waitFor(() => expect(setPendingAttachments).toHaveBeenCalled());
    const update = setPendingAttachments.mock.calls[0]?.[0] as (
      prev: PromptAttachmentInput[],
    ) => PromptAttachmentInput[];
    expect(update([])).toEqual([
      {
        kind: "image",
        mimeType: "image/png",
        name: "shot.png",
        dataB64: "AQID",
      },
    ]);
  });

  it("does not intercept text-only paste", () => {
    const setPendingAttachments = vi.fn();
    const { textarea } = mountComposer(setPendingAttachments);

    expect(
      fireEvent.paste(textarea, {
        clipboardData: {
          items: [],
          getData: (type: string) => (type === "text/plain" ? "plain text" : ""),
        },
      }),
    ).toBe(true);
    expect(setPendingAttachments).not.toHaveBeenCalled();
  });
});
