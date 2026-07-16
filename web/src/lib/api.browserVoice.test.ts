import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { BROWSER_VOICE_MAX_AUDIO_BYTES, invokePluginBrowserVoiceInput } from "./api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("invokePluginBrowserVoiceInput", () => {
  it("sends only the opaque capture id, scoped metadata, and encoded audio", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ ok: true, baseline_revision: 7 }), {
        status: 202,
        headers: { "content-type": "application/json" },
      }),
    );
    const signal = new AbortController().signal;

    const result = await invokePluginBrowserVoiceInput(
      "dev.example/voice",
      "voice.transcribe",
      "session-1",
      "capture-1",
      new Blob([new Uint8Array([1, 2, 3])], { type: "audio/webm;codecs=opus" }),
      1_234.2,
      { language: "en" },
      signal,
    );

    expect(result).toEqual({ kind: "ok", accepted: { baselineRevision: 7 } });
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/plugins/dev.example%2Fvoice/browser-voice-input");
    expect(init?.method).toBe("POST");
    expect(init?.signal).toBe(signal);
    const body = JSON.parse(init!.body as string);
    expect(body).toEqual({
      method: "voice.transcribe",
      params: { language: "en" },
      session_id: "session-1",
      capture_id: "capture-1",
      audio: {
        mime_type: "audio/webm;codecs=opus",
        duration_ms: 1_235,
        data_base64: "AQID",
      },
    });
    expect(JSON.stringify(body)).not.toContain("composer");
  });

  it("surfaces a server rejection message", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ message: "Unsupported browser voice input audio type" }), {
        status: 415,
        headers: { "content-type": "application/json" },
      }),
    );

    await expect(
      invokePluginBrowserVoiceInput(
        "dev.example.voice",
        "voice.transcribe",
        "session-1",
        "capture-1",
        new Blob(["abc"], { type: "audio/aac" }),
        500,
      ),
    ).resolves.toEqual({ kind: "error", message: "Unsupported browser voice input audio type" });
  });

  it("rejects oversized audio before allocating or sending base64", async () => {
    const audio = new Blob([new Uint8Array(BROWSER_VOICE_MAX_AUDIO_BYTES + 1)], { type: "audio/webm" });
    await expect(
      invokePluginBrowserVoiceInput("dev.example.voice", "voice.transcribe", "session-1", "capture-1", audio, 500),
    ).resolves.toEqual({
      kind: "error",
      message: "The recording exceeded the 8 MiB limit. Try a shorter dictation.",
    });
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});
