// @vitest-environment jsdom

import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import { cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { PluginUiEntry } from "../../lib/api";
import { toastBus } from "../../lib/toastBus";
import { registerBrowserVoiceAnchor } from "../plugin/composerDraftOperation";
import { Composer } from "./Composer";

const { entriesRef, pokeMock } = vi.hoisted(() => ({
  entriesRef: { current: [] as PluginUiEntry[] },
  pokeMock: vi.fn(),
}));

vi.mock("../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => entriesRef.current,
  usePluginUiPoke: () => pokeMock,
  usePluginUiRefreshing: () => false,
  usePluginUiRevision: () => 0,
}));

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

function set(entries: PluginUiEntry[]) {
  entriesRef.current = entries;
}

function composerEntry(draftOperation: Record<string, unknown>): PluginUiEntry {
  return {
    plugin_id: "acme.voice",
    slot: "composer-action",
    id: "dictate",
    session_id: "sess-plugin",
    payload: {
      label: "Voice",
      method: "voice.start",
      draft_operation: draftOperation,
    },
  };
}

beforeEach(() => {
  window.localStorage.clear();
  entriesRef.current = [];
  pokeMock.mockClear();
  toastBus.handler = null;
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
  toastBus.handler = null;
});

describe("Composer plugin composer actions", () => {
  it("applies each plugin draft operation id once", async () => {
    set([composerEntry({ kind: "insert-text", id: "op-1", text: "hello" })]);
    const { container, rerender } = render(<HarnessComposer sessionId="sess-plugin" />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    await waitFor(() => expect(textarea.value).toBe("hello"));

    rerender(<HarnessComposer sessionId="sess-plugin" />);
    await waitFor(() => expect(textarea.value).toBe("hello"));

    set([composerEntry({ kind: "insert-text", id: "op-2", text: " world" })]);
    rerender(<HarnessComposer sessionId="sess-plugin" />);
    await waitFor(() => expect(textarea.value).toBe("hello world"));
  });

  it("applies a captured replacement at the initiating browser's exact selection", async () => {
    set([composerEntry({ kind: "set-text", id: "seed", text: "hello world" })]);
    const { container, rerender } = render(<HarnessComposer sessionId="sess-plugin" />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");
    await waitFor(() => expect(textarea.value).toBe("hello world"));

    registerBrowserVoiceAnchor(
      "capture-success",
      { pluginId: "acme.voice", actionId: "dictate", sessionId: "sess-plugin" },
      { expectedText: "hello world", selectionStart: 6, selectionEnd: 11 },
    );
    set([
      composerEntry({
        kind: "replace-selection",
        id: "transcript",
        text: "AoE",
        capture_id: "capture-success",
      }),
    ]);
    rerender(<HarnessComposer sessionId="sess-plugin" />);

    await waitFor(() => expect(textarea.value).toBe("hello AoE"));
    await waitFor(() => {
      expect(textarea.selectionStart).toBe(9);
      expect(textarea.selectionEnd).toBe(9);
    });
  });

  it("keeps newer edits and reports a conflict for a delayed captured replacement", async () => {
    const error = vi.fn();
    toastBus.handler = { push: vi.fn(), error, info: vi.fn() };
    set([composerEntry({ kind: "set-text", id: "seed", text: "original" })]);
    const { container, rerender } = render(<HarnessComposer sessionId="sess-plugin" />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");
    await waitFor(() => expect(textarea.value).toBe("original"));

    registerBrowserVoiceAnchor(
      "capture-conflict",
      { pluginId: "acme.voice", actionId: "dictate", sessionId: "sess-plugin" },
      { expectedText: "original", selectionStart: 8, selectionEnd: 8 },
    );
    set([composerEntry({ kind: "set-text", id: "newer", text: "newer edit" })]);
    rerender(<HarnessComposer sessionId="sess-plugin" />);
    await waitFor(() => expect(textarea.value).toBe("newer edit"));

    set([
      composerEntry({
        kind: "replace-selection",
        id: "transcript",
        text: " dictated",
        capture_id: "capture-conflict",
      }),
    ]);
    rerender(<HarnessComposer sessionId="sess-plugin" />);
    await waitFor(() => expect(error).toHaveBeenCalledWith(expect.stringContaining("newer edits were kept")));
    expect(textarea.value).toBe("newer edit");
  });

  it("ignores a captured operation on a browser that has no matching local anchor", async () => {
    set([
      composerEntry({
        kind: "replace-selection",
        id: "other-browser",
        text: "must not appear",
        capture_id: "unknown-capture",
      }),
    ]);
    const { container } = render(<HarnessComposer sessionId="sess-plugin" />);
    const textarea = container.querySelector("textarea");
    if (!textarea) throw new Error("composer textarea not rendered");

    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(textarea.value).toBe("");
  });
});
