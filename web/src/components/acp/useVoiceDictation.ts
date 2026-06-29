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

interface TranscriptionStatusResponse {
  available?: boolean;
}

interface TranscriptionResponse {
  text?: string;
}

let serverTranscriptionStatusPromise: Promise<boolean> | null = null;

export function getSpeechRecognitionConstructor(): SpeechRecognitionConstructorLike | null {
  if (typeof window === "undefined") return null;
  const speechWindow = window as SpeechRecognitionWindow;
  return speechWindow.SpeechRecognition ?? speechWindow.webkitSpeechRecognition ?? null;
}

export function getMediaRecordingSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof MediaRecorder !== "undefined" &&
    typeof navigator !== "undefined" &&
    typeof navigator.mediaDevices?.getUserMedia === "function"
  );
}

export function resetVoiceDictationServerStatusForTests(): void {
  serverTranscriptionStatusPromise = null;
}

async function getServerTranscriptionAvailable(): Promise<boolean> {
  serverTranscriptionStatusPromise ??= fetch("/api/transcription/status")
    .then(async (response) => {
      if (!response.ok) return false;
      const status = (await response.json()) as TranscriptionStatusResponse;
      return status.available === true;
    })
    .catch(() => false);
  return serverTranscriptionStatusPromise;
}

function mediaRecorderOptions(): MediaRecorderOptions | undefined {
  const preferredTypes = ["audio/webm;codecs=opus", "audio/webm", "audio/mp4", "audio/mpeg"];
  const mimeType = preferredTypes.find((type) => MediaRecorder.isTypeSupported?.(type));
  return mimeType ? { mimeType } : undefined;
}

async function transcribeAudio(blob: Blob): Promise<string> {
  const response = await fetch("/api/transcription", {
    method: "POST",
    headers: blob.type ? { "Content-Type": blob.type } : undefined,
    body: blob,
  });
  if (!response.ok) throw new Error(`transcription failed: ${response.status}`);
  const body = (await response.json()) as TranscriptionResponse;
  const text = body.text?.trim();
  if (!text) throw new Error("empty transcription");
  return text;
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
  processing: boolean;
  error: string | null;
  start: () => void;
  stop: () => void;
}

export function useVoiceDictation(onTranscript: (text: string) => void): VoiceDictationState {
  const onTranscriptRef = useRef(onTranscript);
  const recognitionRef = useRef<SpeechRecognitionLike | null>(null);
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const mediaStreamRef = useRef<MediaStream | null>(null);
  const audioChunksRef = useRef<Blob[]>([]);
  const transcriptSegmentsRef = useRef<string[]>([]);
  const latestTranscriptRef = useRef<string | null>(null);
  const lastEmittedTranscriptRef = useRef<string | null>(null);
  const [browserListening, setBrowserListening] = useState(false);
  const [serverRecording, setServerRecording] = useState(false);
  const [processing, setProcessing] = useState(false);
  const [serverTranscriptionAvailable, setServerTranscriptionAvailable] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const browserSupported = getSpeechRecognitionConstructor() !== null;
  const mediaSupported = getMediaRecordingSupported();
  const supported = serverTranscriptionAvailable || browserSupported;
  const listening = browserListening || serverRecording;

  useEffect(() => {
    onTranscriptRef.current = onTranscript;
  }, [onTranscript]);

  useEffect(() => {
    if (!mediaSupported) return;
    let cancelled = false;
    void getServerTranscriptionAvailable().then((available) => {
      if (!cancelled) setServerTranscriptionAvailable(available);
    });
    return () => {
      cancelled = true;
    };
  }, [mediaSupported]);

  const stop = useCallback(() => {
    const recorder = mediaRecorderRef.current;
    if (recorder && recorder.state === "recording") {
      recorder.stop();
      setServerRecording(false);
      return;
    }
    recognitionRef.current?.stop();
  }, []);

  const cleanupMediaRecording = useCallback(() => {
    mediaStreamRef.current?.getTracks().forEach((track) => track.stop());
    mediaStreamRef.current = null;
    mediaRecorderRef.current = null;
    audioChunksRef.current = [];
    setServerRecording(false);
  }, []);

  const startBrowserRecognition = useCallback(() => {
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
      setBrowserListening(false);
      if (fallback && fallback !== lastEmittedTranscriptRef.current) {
        lastEmittedTranscriptRef.current = fallback;
        onTranscriptRef.current(fallback);
      }
    };

    try {
      recognition.start();
      setError(null);
      setBrowserListening(true);
    } catch {
      recognitionRef.current = null;
      setBrowserListening(false);
      setError("start-failed");
    }
  }, []);

  const startServerRecording = useCallback(async () => {
    if (mediaRecorderRef.current || processing) return;
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      const recorder = new MediaRecorder(stream, mediaRecorderOptions());
      mediaStreamRef.current = stream;
      mediaRecorderRef.current = recorder;
      audioChunksRef.current = [];

      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) audioChunksRef.current.push(event.data);
      };
      recorder.onerror = () => {
        setError("recording-error");
        cleanupMediaRecording();
      };
      recorder.onstop = () => {
        const mimeType = recorder.mimeType || audioChunksRef.current[0]?.type || "audio/webm";
        const audio = new Blob(audioChunksRef.current, { type: mimeType });
        cleanupMediaRecording();
        if (audio.size === 0) {
          setError("empty-recording");
          return;
        }
        setProcessing(true);
        void transcribeAudio(audio)
          .then((text) => {
            setError(null);
            onTranscriptRef.current(text);
          })
          .catch(() => {
            setError("transcription-failed");
          })
          .finally(() => {
            setProcessing(false);
          });
      };

      recorder.start();
      setError(null);
      setServerRecording(true);
    } catch {
      cleanupMediaRecording();
      setError("microphone-unavailable");
    }
  }, [cleanupMediaRecording, processing]);

  const start = useCallback(() => {
    if (serverTranscriptionAvailable && mediaSupported) {
      void startServerRecording();
      return;
    }
    startBrowserRecognition();
  }, [mediaSupported, serverTranscriptionAvailable, startBrowserRecognition, startServerRecording]);

  useEffect(() => {
    return () => {
      recognitionRef.current?.abort?.();
      recognitionRef.current = null;
      cleanupMediaRecording();
      transcriptSegmentsRef.current = [];
      latestTranscriptRef.current = null;
      lastEmittedTranscriptRef.current = null;
    };
  }, [cleanupMediaRecording]);

  return { supported, listening, processing, error, start, stop };
}
