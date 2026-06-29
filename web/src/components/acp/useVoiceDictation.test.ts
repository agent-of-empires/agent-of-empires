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
  return { stopTrack };
}

beforeEach(() => {
  MockSpeechRecognition.instances = [];
  MockMediaRecorder.instances = [];
  resetVoiceDictationServerStatusForTests();
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
        json: async () => ({ available: true, provider: "openai", model: "gpt-4o-mini-transcribe" }),
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
