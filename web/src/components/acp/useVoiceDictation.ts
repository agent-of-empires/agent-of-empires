import { useCallback, useEffect, useRef, useState } from "react";

import { safeGetItem, safeSetItem } from "../../lib/safeStorage";

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
type AudioContextWindow = Window &
  typeof globalThis & {
    webkitAudioContext?: typeof AudioContext;
  };

interface TranscriptionStatusResponse {
  available?: boolean;
}

interface TranscriptionResponse {
  text?: string;
}

let serverTranscriptionStatusPromise: Promise<boolean> | null = null;
const ENHANCED_DICTATION_CONSENT_KEY = "aoe.voiceDictation.enhancedConsent";

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

function disableServerTranscriptionForSession(): void {
  serverTranscriptionStatusPromise = Promise.resolve(false);
}

function hasEnhancedDictationConsent(): boolean {
  return safeGetItem(ENHANCED_DICTATION_CONSENT_KEY) === "1";
}

function requestEnhancedDictationConsent(): boolean {
  if (hasEnhancedDictationConsent()) return true;
  const accepted = window.confirm(
    "Enhanced voice dictation sends recorded audio to this AoE server for OpenAI transcription using the server owner's API key. Continue?",
  );
  if (!accepted) return false;
  safeSetItem(ENHANCED_DICTATION_CONSENT_KEY, "1");
  return true;
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
  audioLevel: number | null;
  error: string | null;
  start: () => void;
  stop: () => void;
}

export function useVoiceDictation(onTranscript: (text: string) => void): VoiceDictationState {
  const onTranscriptRef = useRef(onTranscript);
  const recognitionRef = useRef<SpeechRecognitionLike | null>(null);
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const mediaStreamRef = useRef<MediaStream | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);
  const audioLevelFrameRef = useRef<number | null>(null);
  const audioChunksRef = useRef<Blob[]>([]);
  const serverRecordingStartingRef = useRef(false);
  const discardRecordingRef = useRef(false);
  const browserSessionActiveRef = useRef(false);
  const browserRestartTimerRef = useRef<number | null>(null);
  const startBrowserRecognitionInstanceRef = useRef<() => void>(() => {});
  const browserCommittedTranscriptRef = useRef("");
  const currentRecognitionTranscriptRef = useRef<string | null>(null);
  const transcriptSegmentsRef = useRef<string[]>([]);
  const latestTranscriptRef = useRef<string | null>(null);
  const lastEmittedTranscriptRef = useRef<string | null>(null);
  const [browserListening, setBrowserListening] = useState(false);
  const [serverRecordingStarting, setServerRecordingStarting] = useState(false);
  const [serverRecording, setServerRecording] = useState(false);
  const [processing, setProcessing] = useState(false);
  const [audioLevel, setAudioLevel] = useState<number | null>(null);
  const [serverTranscriptionAvailable, setServerTranscriptionAvailable] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const browserSupported = getSpeechRecognitionConstructor() !== null;
  const mediaSupported = getMediaRecordingSupported();
  const supported = serverTranscriptionAvailable || browserSupported;
  const listening = browserListening || serverRecordingStarting || serverRecording;

  useEffect(() => {
    onTranscriptRef.current = onTranscript;
  }, [onTranscript]);

  const clearBrowserRestartTimer = useCallback(() => {
    if (browserRestartTimerRef.current == null) return;
    window.clearTimeout(browserRestartTimerRef.current);
    browserRestartTimerRef.current = null;
  }, []);

  const emitBrowserTranscript = useCallback((currentRecognitionTranscript: string) => {
    const cleaned = mergeSpeechRecognitionSegments([
      browserCommittedTranscriptRef.current,
      currentRecognitionTranscript,
    ]);
    latestTranscriptRef.current = cleaned || null;
    if (cleaned && cleaned !== lastEmittedTranscriptRef.current) {
      lastEmittedTranscriptRef.current = cleaned;
      onTranscriptRef.current(cleaned);
    }
  }, []);

  const cleanupAudioLevelMeter = useCallback(() => {
    if (audioLevelFrameRef.current != null) {
      window.cancelAnimationFrame(audioLevelFrameRef.current);
      audioLevelFrameRef.current = null;
    }
    const context = audioContextRef.current;
    audioContextRef.current = null;
    if (context && context.state !== "closed") {
      void context.close().catch(() => {});
    }
    setAudioLevel(null);
  }, []);

  const startAudioLevelMeter = useCallback(
    (stream: MediaStream) => {
      cleanupAudioLevelMeter();
      const audioWindow = window as AudioContextWindow;
      const AudioContextCtor = window.AudioContext ?? audioWindow.webkitAudioContext;
      if (!AudioContextCtor) {
        setAudioLevel(null);
        return;
      }

      try {
        const context = new AudioContextCtor();
        const analyser = context.createAnalyser();
        analyser.fftSize = 256;
        const source = context.createMediaStreamSource(stream);
        source.connect(analyser);
        const samples = new Uint8Array(analyser.fftSize);
        audioContextRef.current = context;

        const tick = () => {
          analyser.getByteTimeDomainData(samples);
          let sum = 0;
          for (const sample of samples) {
            const centered = sample - 128;
            sum += centered * centered;
          }
          const rms = Math.sqrt(sum / samples.length);
          setAudioLevel(Math.min(1, rms / 42));
          audioLevelFrameRef.current = window.requestAnimationFrame(tick);
        };

        void context.resume().catch(() => {});
        tick();
      } catch {
        cleanupAudioLevelMeter();
      }
    },
    [cleanupAudioLevelMeter],
  );

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
    if (serverRecordingStartingRef.current && !mediaRecorderRef.current) {
      discardRecordingRef.current = true;
      serverRecordingStartingRef.current = false;
      setServerRecordingStarting(false);
      setServerRecording(false);
      return;
    }
    const recorder = mediaRecorderRef.current;
    if (recorder && recorder.state === "recording") {
      discardRecordingRef.current = false;
      setProcessing(true);
      recorder.stop();
      setServerRecording(false);
      return;
    }
    browserSessionActiveRef.current = false;
    clearBrowserRestartTimer();
    const recognition = recognitionRef.current;
    if (recognition) {
      recognition.stop();
      return;
    }
    setBrowserListening(false);
    transcriptSegmentsRef.current = [];
    currentRecognitionTranscriptRef.current = null;
    latestTranscriptRef.current = null;
    lastEmittedTranscriptRef.current = null;
    browserCommittedTranscriptRef.current = "";
  }, [clearBrowserRestartTimer]);

  const cleanupMediaRecording = useCallback(
    (discard = true) => {
      const recorder = mediaRecorderRef.current;
      if (discard) discardRecordingRef.current = true;
      if (recorder) {
        recorder.ondataavailable = null;
        recorder.onerror = null;
        recorder.onstop = null;
        if (discard && recorder.state === "recording") {
          try {
            recorder.stop();
          } catch {
            // ignore: the browser may already be stopping the recorder
          }
        }
      }
      mediaStreamRef.current?.getTracks().forEach((track) => track.stop());
      mediaStreamRef.current = null;
      mediaRecorderRef.current = null;
      audioChunksRef.current = [];
      cleanupAudioLevelMeter();
      serverRecordingStartingRef.current = false;
      setServerRecordingStarting(false);
      setServerRecording(false);
    },
    [cleanupAudioLevelMeter],
  );

  const startBrowserRecognitionInstance = useCallback(() => {
    if (recognitionRef.current) return;
    const Recognition = getSpeechRecognitionConstructor();
    if (!Recognition) {
      browserSessionActiveRef.current = false;
      setError("unsupported");
      return;
    }

    const recognition = new Recognition();
    recognition.continuous = true;
    recognition.interimResults = true;
    recognition.lang = navigator.language || "en-US";
    recognitionRef.current = recognition;
    transcriptSegmentsRef.current = [];
    currentRecognitionTranscriptRef.current = null;

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
      currentRecognitionTranscriptRef.current = cleaned || null;
      if (cleaned) emitBrowserTranscript(cleaned);
    };
    recognition.onerror = (event) => {
      const reason = event.error ?? "speech-recognition-error";
      if (["audio-capture", "language-not-supported", "not-allowed", "service-not-allowed"].includes(reason)) {
        browserSessionActiveRef.current = false;
        setError(reason);
      } else if (!browserSessionActiveRef.current) {
        setError(reason);
      }
    };
    recognition.onend = () => {
      const currentRecognitionTranscript = currentRecognitionTranscriptRef.current;
      if (currentRecognitionTranscript) {
        browserCommittedTranscriptRef.current = mergeSpeechRecognitionSegments([
          browserCommittedTranscriptRef.current,
          currentRecognitionTranscript,
        ]);
      }
      transcriptSegmentsRef.current = [];
      currentRecognitionTranscriptRef.current = null;
      recognitionRef.current = null;
      if (browserSessionActiveRef.current) {
        setBrowserListening(true);
        clearBrowserRestartTimer();
        browserRestartTimerRef.current = window.setTimeout(() => {
          browserRestartTimerRef.current = null;
          startBrowserRecognitionInstanceRef.current();
        }, 100);
        return;
      }
      const fallback = browserCommittedTranscriptRef.current || latestTranscriptRef.current;
      if (fallback && fallback !== lastEmittedTranscriptRef.current) {
        lastEmittedTranscriptRef.current = fallback;
        onTranscriptRef.current(fallback);
      }
      setBrowserListening(false);
      latestTranscriptRef.current = null;
      browserCommittedTranscriptRef.current = "";
    };

    try {
      recognition.start();
      setError(null);
      setBrowserListening(true);
    } catch {
      recognitionRef.current = null;
      browserSessionActiveRef.current = false;
      setBrowserListening(false);
      setError("start-failed");
    }
  }, [clearBrowserRestartTimer, emitBrowserTranscript]);

  useEffect(() => {
    startBrowserRecognitionInstanceRef.current = startBrowserRecognitionInstance;
  }, [startBrowserRecognitionInstance]);

  const startBrowserRecognition = useCallback(() => {
    if (browserSessionActiveRef.current || recognitionRef.current) return;
    browserSessionActiveRef.current = true;
    clearBrowserRestartTimer();
    browserCommittedTranscriptRef.current = "";
    transcriptSegmentsRef.current = [];
    currentRecognitionTranscriptRef.current = null;
    latestTranscriptRef.current = null;
    lastEmittedTranscriptRef.current = null;
    startBrowserRecognitionInstance();
  }, [clearBrowserRestartTimer, startBrowserRecognitionInstance]);

  const startServerRecording = useCallback(async () => {
    if (serverRecordingStartingRef.current || mediaRecorderRef.current || processing) return;
    serverRecordingStartingRef.current = true;
    setServerRecordingStarting(true);
    discardRecordingRef.current = false;
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      mediaStreamRef.current = stream;
      if (discardRecordingRef.current) {
        stream.getTracks().forEach((track) => track.stop());
        mediaStreamRef.current = null;
        serverRecordingStartingRef.current = false;
        setServerRecordingStarting(false);
        return;
      }
      startAudioLevelMeter(stream);
      const recorder = new MediaRecorder(stream, mediaRecorderOptions());
      mediaRecorderRef.current = recorder;
      audioChunksRef.current = [];

      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) audioChunksRef.current.push(event.data);
      };
      recorder.onerror = () => {
        setError("recording-error");
        setProcessing(false);
        cleanupMediaRecording(true);
      };
      recorder.onstop = () => {
        if (discardRecordingRef.current) {
          cleanupMediaRecording(true);
          return;
        }
        const mimeType = recorder.mimeType || audioChunksRef.current[0]?.type || "audio/webm";
        const audio = new Blob(audioChunksRef.current, { type: mimeType });
        cleanupMediaRecording(false);
        if (audio.size === 0) {
          setError("empty-recording");
          setProcessing(false);
          return;
        }
        void transcribeAudio(audio)
          .then((text) => {
            setError(null);
            onTranscriptRef.current(text);
          })
          .catch(() => {
            disableServerTranscriptionForSession();
            setServerTranscriptionAvailable(false);
            if (browserSupported) {
              startBrowserRecognition();
            } else {
              setError("transcription-failed");
            }
          })
          .finally(() => {
            setProcessing(false);
          });
      };

      recorder.start();
      setError(null);
      serverRecordingStartingRef.current = false;
      setServerRecordingStarting(false);
      setServerRecording(true);
    } catch {
      cleanupMediaRecording(true);
      setProcessing(false);
      setError("microphone-unavailable");
    }
  }, [browserSupported, cleanupMediaRecording, processing, startAudioLevelMeter, startBrowserRecognition]);

  const start = useCallback(() => {
    if (serverTranscriptionAvailable && mediaSupported) {
      if (!requestEnhancedDictationConsent()) {
        setError("enhanced-consent-declined");
        return;
      }
      void startServerRecording();
      return;
    }
    startBrowserRecognition();
  }, [mediaSupported, serverTranscriptionAvailable, startBrowserRecognition, startServerRecording]);

  useEffect(() => {
    return () => {
      browserSessionActiveRef.current = false;
      clearBrowserRestartTimer();
      recognitionRef.current?.abort?.();
      recognitionRef.current = null;
      cleanupMediaRecording(true);
      transcriptSegmentsRef.current = [];
      currentRecognitionTranscriptRef.current = null;
      latestTranscriptRef.current = null;
      lastEmittedTranscriptRef.current = null;
      browserCommittedTranscriptRef.current = "";
    };
  }, [cleanupMediaRecording, clearBrowserRestartTimer]);

  return { supported, listening, processing, audioLevel, error, start, stop };
}
