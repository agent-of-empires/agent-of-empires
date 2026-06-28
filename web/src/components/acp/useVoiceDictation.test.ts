// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import { mergeSpeechRecognitionSegments, useVoiceDictation, type SpeechRecognitionLike } from "./useVoiceDictation";

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

beforeEach(() => {
  MockSpeechRecognition.instances = [];
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
    const onTranscript = vi.fn();
    const { result } = renderHook(() => useVoiceDictation(onTranscript));

    act(() => result.current.start());

    expect(result.current.supported).toBe(false);
    expect(result.current.error).toBe("unsupported");
    expect(onTranscript).not.toHaveBeenCalled();
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
