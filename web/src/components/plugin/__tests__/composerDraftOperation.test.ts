// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { PluginUiEntry } from "../../../lib/api";
import {
  composerDraftOperation,
  consumeBrowserVoiceAnchor,
  registerBrowserVoiceAnchor,
  removeBrowserVoiceAnchor,
} from "../composerDraftOperation";

const scope = { pluginId: "acme.voice", actionId: "dictate", sessionId: "session-1" };

afterEach(() => {
  vi.useRealTimers();
});

describe("browser-local voice anchors", () => {
  it("is scope-bound and can only be consumed once", () => {
    registerBrowserVoiceAnchor("capture-once", scope, {
      expectedText: "private draft",
      selectionStart: 2,
      selectionEnd: 5,
    });

    expect(
      consumeBrowserVoiceAnchor("capture-once", {
        ...scope,
        pluginId: "other.plugin",
      }),
    ).toBeNull();
    expect(consumeBrowserVoiceAnchor("capture-once", scope)).toEqual({
      expectedText: "private draft",
      selectionStart: 2,
      selectionEnd: 5,
    });
    expect(consumeBrowserVoiceAnchor("capture-once", scope)).toBeNull();
  });

  it("expires even when no later anchor operation triggers a sweep", async () => {
    vi.useFakeTimers();
    registerBrowserVoiceAnchor("capture-expiry", scope, {
      expectedText: "private draft",
      selectionStart: 0,
      selectionEnd: 0,
    });

    await vi.advanceTimersByTimeAsync(10 * 60 * 1000);
    expect(consumeBrowserVoiceAnchor("capture-expiry", scope)).toBeNull();
  });

  it("evicts the oldest anchor at the hard 128-entry bound", () => {
    vi.useFakeTimers();
    for (let index = 0; index < 129; index += 1) {
      registerBrowserVoiceAnchor(`bounded-${index}`, scope, {
        expectedText: `draft-${index}`,
        selectionStart: 0,
        selectionEnd: 0,
      });
    }

    expect(consumeBrowserVoiceAnchor("bounded-0", scope)).toBeNull();
    expect(consumeBrowserVoiceAnchor("bounded-128", scope)?.expectedText).toBe("draft-128");
    for (let index = 1; index < 128; index += 1) removeBrowserVoiceAnchor(`bounded-${index}`);
  });

  it("parses only an opaque capture id, never draft text or coordinates", () => {
    const entry: PluginUiEntry = {
      plugin_id: "acme.voice",
      slot: "composer-action",
      id: "dictate",
      session_id: "session-1",
      payload: {
        label: "Voice",
        method: "voice.transcribe",
        draft_operation: {
          kind: "replace-selection",
          id: "transcript-1",
          text: "hello",
          capture_id: "capture-1",
        },
      },
    };

    expect(composerDraftOperation(entry)).toEqual({
      id: "transcript-1",
      operation: { kind: "replace-selection", text: "hello", captureId: "capture-1" },
    });
  });
});
