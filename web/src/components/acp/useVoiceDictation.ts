import { useCallback, useEffect, useRef, useState } from "react";

type SpeechRecognitionAlternativeLike = { transcript: string };
type SpeechRecognitionResultLike = {
  readonly isFinal: boolean;
  readonly 0: SpeechRecognitionAlternativeLike;
};
type SpeechRecognitionResultListLike = {
  readonly length: number;
  readonly [index: number]: SpeechRecognitionResultLike;
};
type SpeechRecognitionResultEventLike = {
  readonly resultIndex: number;
  readonly results: SpeechRecognitionResultListLike;
};
type SpeechRecognitionErrorEventLike = { readonly error?: string };

export interface SpeechRecognitionLike {
  continuous: boolean;
  interimResults: boolean;
  lang: string;
  onresult: ((event: SpeechRecognitionResultEventLike) => void) | null;
  onerror: ((event: SpeechRecognitionErrorEventLike) => void) | null;
  onend: (() => void) | null;
  start: () => void;
  stop: () => void;
  abort?: () => void;
}

export type SpeechRecognitionConstructorLike = new () => SpeechRecognitionLike;

type SpeechRecognitionWindow = Window &
  typeof globalThis & {
    SpeechRecognition?: SpeechRecognitionConstructorLike;
    webkitSpeechRecognition?: SpeechRecognitionConstructorLike;
  };

export function getSpeechRecognitionConstructor(): SpeechRecognitionConstructorLike | null {
  if (typeof window === "undefined") return null;
  const speechWindow = window as SpeechRecognitionWindow;
  return speechWindow.SpeechRecognition ?? speechWindow.webkitSpeechRecognition ?? null;
}

function normalizeTranscriptSegment(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

function isBoundaryPrefix(previous: string, next: string): boolean {
  const previousLower = previous.toLocaleLowerCase();
  const nextLower = next.toLocaleLowerCase();
  if (!nextLower.startsWith(previousLower)) return false;
  const boundary = next.charAt(previous.length);
  return boundary === "" || !/[A-Za-z0-9]/.test(boundary);
}

function findWordOverlap(previous: string, next: string): number {
  const previousWords = previous.split(/\s+/);
  const nextWords = next.split(/\s+/);
  const maxOverlap = Math.min(previousWords.length, nextWords.length);

  for (let overlap = maxOverlap; overlap > 0; overlap -= 1) {
    const previousSuffix = previousWords
      .slice(previousWords.length - overlap)
      .join(" ")
      .toLocaleLowerCase();
    const nextPrefix = nextWords.slice(0, overlap).join(" ").toLocaleLowerCase();
    if (previousSuffix === nextPrefix) return overlap;
  }

  return 0;
}

export function mergeSpeechRecognitionSegments(segments: string[]): string {
  let merged = "";

  for (const segment of segments) {
    const next = normalizeTranscriptSegment(segment);
    if (!next) continue;

    if (!merged) {
      merged = next;
      continue;
    }

    if (isBoundaryPrefix(merged, next)) {
      merged = next;
      continue;
    }

    const overlap = findWordOverlap(merged, next);
    if (overlap > 0) {
      const nextRemainder = next.split(/\s+/).slice(overlap).join(" ");
      merged = nextRemainder ? `${merged} ${nextRemainder}` : merged;
      continue;
    }

    merged = `${merged} ${next}`;
  }

  return merged.trim();
}

export interface VoiceDictationState {
  supported: boolean;
  listening: boolean;
  error: string | null;
  start: () => void;
  stop: () => void;
}

export function useVoiceDictation(onTranscript: (text: string) => void): VoiceDictationState {
  const onTranscriptRef = useRef(onTranscript);
  const recognitionRef = useRef<SpeechRecognitionLike | null>(null);
  const transcriptSegmentsRef = useRef<string[]>([]);
  const latestTranscriptRef = useRef<string | null>(null);
  const lastEmittedTranscriptRef = useRef<string | null>(null);
  const [listening, setListening] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const supported = getSpeechRecognitionConstructor() !== null;

  useEffect(() => {
    onTranscriptRef.current = onTranscript;
  }, [onTranscript]);

  const stop = useCallback(() => {
    recognitionRef.current?.stop();
  }, []);

  const start = useCallback(() => {
    if (recognitionRef.current) return;
    const Recognition = getSpeechRecognitionConstructor();
    if (!Recognition) {
      setError("unsupported");
      return;
    }

    const recognition = new Recognition();
    recognition.continuous = true;
    recognition.interimResults = true;
    recognition.lang = navigator.language || "en-US";
    recognitionRef.current = recognition;
    transcriptSegmentsRef.current = [];
    latestTranscriptRef.current = null;
    lastEmittedTranscriptRef.current = null;

    recognition.onresult = (event) => {
      const segments = transcriptSegmentsRef.current;
      for (let i = 0; i < event.results.length; i += 1) {
        const result = event.results[i];
        if (!result) continue;
        const transcript = result?.[0]?.transcript ?? "";
        segments[i] = transcript;
      }
      segments.length = event.results.length;

      const cleaned = mergeSpeechRecognitionSegments(segments);
      latestTranscriptRef.current = cleaned || null;
      if (cleaned && cleaned !== lastEmittedTranscriptRef.current) {
        lastEmittedTranscriptRef.current = cleaned;
        onTranscriptRef.current(cleaned);
      }
    };
    recognition.onerror = (event) => {
      setError(event.error ?? "speech-recognition-error");
    };
    recognition.onend = () => {
      const fallback = latestTranscriptRef.current;
      transcriptSegmentsRef.current = [];
      latestTranscriptRef.current = null;
      recognitionRef.current = null;
      setListening(false);
      if (fallback && fallback !== lastEmittedTranscriptRef.current) {
        lastEmittedTranscriptRef.current = fallback;
        onTranscriptRef.current(fallback);
      }
    };

    try {
      recognition.start();
      setError(null);
      setListening(true);
    } catch {
      recognitionRef.current = null;
      setListening(false);
      setError("start-failed");
    }
  }, []);

  useEffect(() => {
    return () => {
      recognitionRef.current?.abort?.();
      recognitionRef.current = null;
      transcriptSegmentsRef.current = [];
      latestTranscriptRef.current = null;
      lastEmittedTranscriptRef.current = null;
    };
  }, []);

  return { supported, listening, error, start, stop };
}
