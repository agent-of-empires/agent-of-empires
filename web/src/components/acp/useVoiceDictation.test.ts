// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";

import {
  mergeSpeechRecognitionSegments,
  resetVoiceDictationServerStatusForTests,
  useVoiceDictation,
  type SpeechRecognitionLike,
} from "./useVoiceDictation";

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

  emitResult(results: Array<{ transcript: string; isFinal: boolean }>, resultIndex = 0) {
    const list = results.map((result) => ({
      isFinal: result.isFinal,
      0: { transcript: result.transcript },
    }));
    this.onresult?.({
      resultIndex,
      results: Object.assign(list, { length: list.length }),
    });
  }

  emitEnd() {
    this.onend?.();
  }
}

class MockMediaRecorder {
  static instances: MockMediaRecorder[] = [];
  static isTypeSupported = vi.fn(() => true);

  mimeType: string;
  state: RecordingState = "inactive";
  ondataavailable: ((event: BlobEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onstop: ((event: Event) => void) | null = null;

  constructor(
    public stream: MediaStream,
    options?: MediaRecorderOptions,
  ) {
    this.mimeType = options?.mimeType ?? "audio/webm";
    MockMediaRecorder.instances.push(this);
  }

  start() {
    this.state = "recording";
  }

  stop() {
    this.state = "inactive";
    this.ondataavailable?.({ data: new Blob(["audio"], { type: this.mimeType }) } as BlobEvent);
    this.onstop?.(new Event("stop"));
  }
}

function stubMediaRecording() {
  const stopTrack = vi.fn();
  const stream = { getTracks: () => [{ stop: stopTrack }] } as unknown as MediaStream;
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: {
      getUserMedia: vi.fn().mockResolvedValue(stream),
    },
  });
  vi.stubGlobal("MediaRecorder", MockMediaRecorder);
  vi.stubGlobal(
    "confirm",
    vi.fn(() => true),
  );
  return { stopTrack };
}

beforeEach(() => {
  MockSpeechRecognition.instances = [];
  MockMediaRecorder.instances = [];
  resetVoiceDictationServerStatusForTests();
  window.localStorage.clear();
  vi.stubGlobal("webkitSpeechRecognition", MockSpeechRecognition);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("useVoiceDictation", () => {
  it("configures browser speech recognition and starts listening", () => {
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    act(() => result.current.start());

    const recognition = MockSpeechRecognition.instances[0];
    expect(recognition).toBeTruthy();
    expect(recognition.continuous).toBe(true);
    expect(recognition.interimResults).toBe(true);
    expect(recognition.lang).toBe(navigator.language || "en-US");
    expect(recognition.start).toHaveBeenCalledTimes(1);
    expect(result.current.listening).toBe(true);
  });

  it("emits the full cumulative transcript on each result", () => {
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));
    act(() => result.current.start());

    act(() => {
      MockSpeechRecognition.instances[0]!.emitResult([{ transcript: " build ", isFinal: true }]);
      MockSpeechRecognition.instances[0]!.emitResult([
        { transcript: " build ", isFinal: true },
        { transcript: "the feature ", isFinal: true },
      ]);
    });

    expect(onTranscript).toHaveBeenNthCalledWith(1, "build");
    expect(onTranscript).toHaveBeenNthCalledWith(2, "build the feature");
    expect(onTranscript).toHaveBeenCalledTimes(2);
  });

  it("deduplicates overlapping mobile recognition hypotheses", () => {
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));
    act(() => result.current.start());

    act(() => {
      MockSpeechRecognition.instances[0]!.emitResult([{ transcript: "Alpha", isFinal: false }]);
      MockSpeechRecognition.instances[0]!.emitResult([
        { transcript: "Alpha", isFinal: false },
        { transcript: "Alpha Beta", isFinal: false },
        { transcript: "Alpha Beta Charlie", isFinal: false },
      ]);
    });

    expect(onTranscript).toHaveBeenNthCalledWith(1, "Alpha");
    expect(onTranscript).toHaveBeenNthCalledWith(2, "Alpha Beta Charlie");
    expect(onTranscript).toHaveBeenCalledTimes(2);
  });

  it("flushes the latest interim transcript when recording ends without a final result", () => {
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));
    act(() => result.current.start());

    act(() => {
      MockSpeechRecognition.instances[0]!.emitResult([{ transcript: "drafted phrase", isFinal: false }]);
      result.current.stop();
    });

    expect(onTranscript).toHaveBeenCalledWith("drafted phrase");
    expect(onTranscript).toHaveBeenCalledTimes(1);
    expect(result.current.listening).toBe(false);
  });

  it("restarts browser recognition after a spontaneous end and keeps a cumulative transcript", async () => {
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));
    act(() => result.current.start());

    act(() => {
      MockSpeechRecognition.instances[0]!.emitResult([{ transcript: "review the", isFinal: false }]);
      MockSpeechRecognition.instances[0]!.emitEnd();
    });

    expect(result.current.listening).toBe(true);
    await waitFor(() => expect(MockSpeechRecognition.instances).toHaveLength(2));

    act(() => {
      MockSpeechRecognition.instances[1]!.emitResult([{ transcript: "diff please", isFinal: false }]);
    });

    expect(onTranscript).toHaveBeenNthCalledWith(1, "review the");
    expect(onTranscript).toHaveBeenNthCalledWith(2, "review the diff please");
    expect(result.current.listening).toBe(true);

    act(() => result.current.stop());
    expect(result.current.listening).toBe(false);
  });

  it("reports unsupported browsers without calling the transcript sink", () => {
    vi.unstubAllGlobals();
    resetVoiceDictationServerStatusForTests();
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    act(() => result.current.start());

    expect(result.current.supported).toBe(false);
    expect(result.current.error).toBe("unsupported");
    expect(onTranscript).not.toHaveBeenCalled();
  });

  it("records audio and inserts server transcription when configured", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    const { stopTrack } = stubMediaRecording();
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      })
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ text: "Supabase TypeScript project" }),
      });
    vi.stubGlobal("fetch", fetchMock);
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    await waitFor(() => expect(result.current.supported).toBe(true));

    await act(async () => {
      result.current.start();
      await Promise.resolve();
    });

    expect(MockMediaRecorder.instances).toHaveLength(1);
    expect(result.current.listening).toBe(true);

    await act(async () => {
      result.current.stop();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(stopTrack).toHaveBeenCalled();
    expect(fetchMock).toHaveBeenNthCalledWith(1, "/api/transcription/status");
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/transcription",
      expect.objectContaining({
        method: "POST",
        headers: { "Content-Type": "audio/webm;codecs=opus" },
        body: expect.any(Blob),
      }),
    );
    expect(onTranscript).toHaveBeenCalledWith("Supabase TypeScript project");
  });

  it("enters processing state immediately after enhanced recording stops", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    stubMediaRecording();
    let resolveTranscription!: (response: { ok: boolean; json: () => Promise<{ text: string }> }) => void;
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      })
      .mockReturnValueOnce(
        new Promise((resolve) => {
          resolveTranscription = resolve;
        }),
      );
    vi.stubGlobal("fetch", fetchMock);
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    await waitFor(() => expect(result.current.supported).toBe(true));

    await act(async () => {
      result.current.start();
      await Promise.resolve();
    });

    await act(async () => {
      result.current.stop();
      await Promise.resolve();
    });

    expect(result.current.processing).toBe(true);
    expect(result.current.listening).toBe(false);

    await act(async () => {
      resolveTranscription({
        ok: true,
        json: async () => ({ text: "queued transcription" }),
      });
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(result.current.processing).toBe(false);
    expect(onTranscript).toHaveBeenCalledWith("queued transcription");
  });

  it("does not start enhanced recording when the user declines audio egress consent", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    stubMediaRecording();
    vi.stubGlobal(
      "confirm",
      vi.fn(() => false),
    );
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      }),
    );
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    await waitFor(() => expect(result.current.supported).toBe(true));

    act(() => result.current.start());

    expect(MockMediaRecorder.instances).toHaveLength(0);
    expect(navigator.mediaDevices.getUserMedia).not.toHaveBeenCalled();
    expect(result.current.error).toBe("enhanced-consent-declined");
  });

  it("asks for enhanced recording consent only once", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    const { stopTrack } = stubMediaRecording();
    const confirmMock = vi.fn(() => true);
    vi.stubGlobal("confirm", confirmMock);
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValueOnce({
          ok: true,
          json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
        })
        .mockResolvedValue({
          ok: true,
          json: async () => ({ text: "second pass" }),
        }),
    );
    const { result } = renderHook(() => useVoiceDictation(vi.fn()));

    await waitFor(() => expect(result.current.supported).toBe(true));

    await act(async () => {
      result.current.start();
      await Promise.resolve();
    });
    await act(async () => {
      result.current.stop();
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      result.current.start();
      await Promise.resolve();
    });

    expect(confirmMock).toHaveBeenCalledTimes(1);
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledTimes(2);
    expect(MockMediaRecorder.instances).toHaveLength(2);
    expect(stopTrack).toHaveBeenCalled();
  });

  it("coalesces rapid enhanced recording starts while microphone permission is pending", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    stubMediaRecording();
    let resolveStream!: (stream: MediaStream) => void;
    const stream = { getTracks: () => [{ stop: vi.fn() }] } as unknown as MediaStream;
    vi.mocked(navigator.mediaDevices.getUserMedia).mockReturnValue(
      new Promise<MediaStream>((resolve) => {
        resolveStream = resolve;
      }),
    );
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      }),
    );
    const { result } = renderHook(() => useVoiceDictation(vi.fn()));

    await waitFor(() => expect(result.current.supported).toBe(true));

    act(() => {
      result.current.start();
      result.current.start();
    });
    await act(async () => {
      resolveStream(stream);
      await Promise.resolve();
    });

    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledTimes(1);
    expect(MockMediaRecorder.instances).toHaveLength(1);
  });

  it("cancels enhanced recording while microphone permission is pending", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    stubMediaRecording();
    const stopTrack = vi.fn();
    let resolveStream!: (stream: MediaStream) => void;
    const stream = { getTracks: () => [{ stop: stopTrack }] } as unknown as MediaStream;
    vi.mocked(navigator.mediaDevices.getUserMedia).mockReturnValue(
      new Promise<MediaStream>((resolve) => {
        resolveStream = resolve;
      }),
    );
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      }),
    );
    const { result, unmount } = renderHook(() => useVoiceDictation(vi.fn()));

    await waitFor(() => expect(result.current.supported).toBe(true));

    act(() => {
      result.current.start();
    });
    expect(result.current.listening).toBe(true);

    act(() => {
      result.current.stop();
    });
    expect(result.current.listening).toBe(false);

    await act(async () => {
      resolveStream(stream);
      await Promise.resolve();
    });

    expect(stopTrack).toHaveBeenCalledTimes(1);
    expect(MockMediaRecorder.instances).toHaveLength(0);

    unmount();
    expect(stopTrack).toHaveBeenCalledTimes(1);
  });

  it("stops acquired tracks when MediaRecorder construction fails", async () => {
    vi.stubGlobal("webkitSpeechRecognition", undefined);
    const { stopTrack } = stubMediaRecording();
    vi.stubGlobal(
      "MediaRecorder",
      class {
        static isTypeSupported = vi.fn(() => true);
        constructor() {
          throw new Error("unsupported");
        }
      },
    );
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      }),
    );
    const { result } = renderHook(() => useVoiceDictation(vi.fn()));

    await waitFor(() => expect(result.current.supported).toBe(true));

    await act(async () => {
      result.current.start();
      await Promise.resolve();
    });

    expect(stopTrack).toHaveBeenCalled();
    expect(result.current.error).toBe("microphone-unavailable");
  });

  it("falls back to browser speech after a server transcription failure", async () => {
    stubMediaRecording();
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-transcribe" }),
      })
      .mockResolvedValueOnce({
        ok: false,
        status: 502,
        json: async () => ({}),
      });
    vi.stubGlobal("fetch", fetchMock);
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    await waitFor(() => expect(result.current.supported).toBe(true));

    await act(async () => {
      result.current.start();
      await Promise.resolve();
    });
    await act(async () => {
      result.current.stop();
      await Promise.resolve();
      await Promise.resolve();
    });
    await waitFor(() => expect(MockSpeechRecognition.instances).toHaveLength(1));
    expect(result.current.error).toBeNull();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

describe("mergeSpeechRecognitionSegments", () => {
  it("keeps distinct recognition segments", () => {
    expect(mergeSpeechRecognitionSegments([" build ", "the feature "])).toBe("build the feature");
  });

  it("collapses repeated growing prefixes", () => {
    expect(mergeSpeechRecognitionSegments(["Alpha", "Alpha Beta", "Alpha Beta Charlie"])).toBe("Alpha Beta Charlie");
  });

  it("merges suffix and prefix overlap", () => {
    expect(mergeSpeechRecognitionSegments(["Alpha Beta", "Beta Charlie"])).toBe("Alpha Beta Charlie");
  });
});
