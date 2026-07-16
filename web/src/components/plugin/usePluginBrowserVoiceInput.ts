import { useCallback, useEffect, useRef, useState } from "react";

import {
  BROWSER_VOICE_MAX_AUDIO_BYTES,
  BROWSER_VOICE_MAX_DURATION_MS,
  invokePluginBrowserVoiceInput,
} from "../../lib/api";
import { reportError } from "../../lib/toastBus";
import { registerBrowserVoiceAnchor, removeBrowserVoiceAnchor } from "./composerDraftOperation";

export interface ComposerActionSnapshot {
  text: string;
  selectionStart: number;
  selectionEnd: number;
}

export type BrowserVoicePhase = "idle" | "starting" | "recording" | "uploading" | "error";

export interface BrowserVoiceError {
  label: string;
  message: string;
}

interface ActiveVoiceCapture {
  id: number;
  method: string;
  recorder: MediaRecorder;
  stream: MediaStream;
  chunks: Blob[];
  bytes: number;
  startedAt: number;
  intervalId: number;
  timeoutId: number;
  snapshot: ComposerActionSnapshot | null;
}

function mediaRecorderOptions(): MediaRecorderOptions | undefined {
  const preferredTypes = ["audio/webm;codecs=opus", "audio/webm", "audio/mp4", "audio/mpeg"];
  const mimeType = preferredTypes.find((type) => MediaRecorder.isTypeSupported?.(type));
  return mimeType ? { mimeType } : undefined;
}

function browserVoiceInputSupported(): boolean {
  return (
    typeof MediaRecorder !== "undefined" &&
    typeof navigator !== "undefined" &&
    typeof navigator.mediaDevices?.getUserMedia === "function"
  );
}

function stopStream(stream: MediaStream): void {
  stream.getTracks().forEach((track) => track.stop());
}

function microphoneError(error: unknown): BrowserVoiceError {
  const name = error instanceof DOMException ? error.name : "";
  if (name === "NotAllowedError" || name === "SecurityError") {
    return {
      label: "Microphone blocked",
      message: "Microphone access was denied. Allow microphone access in the browser, then try again.",
    };
  }
  if (name === "NotFoundError" || name === "DevicesNotFoundError") {
    return { label: "No microphone", message: "No microphone is available to this browser." };
  }
  if (name === "NotReadableError" || name === "TrackStartError") {
    return {
      label: "Microphone busy",
      message: "The microphone is already in use or could not be started.",
    };
  }
  return { label: "Recording failed", message: "The browser could not start microphone recording." };
}

export function formatBrowserVoiceElapsed(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function newBrowserVoiceCaptureId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") return crypto.randomUUID();
  if (typeof crypto !== "undefined" && typeof crypto.getRandomValues === "function") {
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    return `voice-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
  }
  throw new Error("Secure random capture ids are unavailable");
}

export function usePluginBrowserVoiceInput({
  enabled,
  pluginId,
  actionId,
  method,
  sessionId,
  getSnapshot,
  onAccepted,
}: {
  enabled: boolean;
  pluginId: string;
  actionId: string;
  method: string | undefined;
  sessionId: string;
  getSnapshot: () => ComposerActionSnapshot;
  onAccepted: () => void;
}) {
  const [phase, setPhase] = useState<BrowserVoicePhase>("idle");
  const [elapsedMs, setElapsedMs] = useState(0);
  const [voiceError, setVoiceError] = useState<BrowserVoiceError | null>(null);
  const phaseRef = useRef<BrowserVoicePhase>("idle");
  const requestIdRef = useRef(0);
  const activeCaptureRef = useRef<ActiveVoiceCapture | null>(null);
  const uploadAbortRef = useRef<AbortController | null>(null);
  const uploadingAnchorRef = useRef<string | null>(null);
  const supported = enabled && browserVoiceInputSupported();

  const setVoicePhase = useCallback((next: BrowserVoicePhase) => {
    phaseRef.current = next;
    setPhase(next);
  }, []);

  const releaseCapture = useCallback((capture: ActiveVoiceCapture, stopRecorder: boolean) => {
    window.clearInterval(capture.intervalId);
    window.clearTimeout(capture.timeoutId);
    capture.recorder.ondataavailable = null;
    capture.recorder.onerror = null;
    capture.recorder.onstop = null;
    if (stopRecorder && capture.recorder.state !== "inactive") {
      try {
        capture.recorder.stop();
      } catch {
        // The recorder can become inactive between the state read and stop().
      }
    }
    stopStream(capture.stream);
    if (activeCaptureRef.current?.id === capture.id) activeCaptureRef.current = null;
  }, []);

  const showError = useCallback(
    (error: BrowserVoiceError) => {
      setVoiceError(error);
      setVoicePhase("error");
      reportError(error.message);
    },
    [setVoicePhase],
  );

  const failCapture = useCallback(
    (captureId: number, error: BrowserVoiceError) => {
      const capture = activeCaptureRef.current;
      if (!capture || capture.id !== captureId) return;
      requestIdRef.current += 1;
      releaseCapture(capture, true);
      showError(error);
    },
    [releaseCapture, showError],
  );

  const finalizeCapture = useCallback(
    async (capture: ActiveVoiceCapture) => {
      if (activeCaptureRef.current?.id !== capture.id || requestIdRef.current !== capture.id) return;
      capture.snapshot ??= getSnapshot();
      setElapsedMs(Math.min(Date.now() - capture.startedAt, BROWSER_VOICE_MAX_DURATION_MS));
      setVoicePhase("uploading");
      releaseCapture(capture, false);
      const mimeType = capture.recorder.mimeType || capture.chunks[0]?.type || "audio/webm";
      const audio = new Blob(capture.chunks, { type: mimeType });
      if (audio.size === 0) {
        showError({ label: "No audio captured", message: "No microphone audio was captured. Please try again." });
        return;
      }
      if (audio.size > BROWSER_VOICE_MAX_AUDIO_BYTES) {
        showError({
          label: "Recording too large",
          message: "The recording exceeded the 8 MiB limit. Try a shorter dictation.",
        });
        return;
      }

      const snapshot = capture.snapshot;
      const durationMs = Math.max(1, Math.min(Date.now() - capture.startedAt, BROWSER_VOICE_MAX_DURATION_MS));
      let captureId: string;
      try {
        captureId = newBrowserVoiceCaptureId();
      } catch {
        showError({
          label: "Recording failed",
          message: "The browser could not create a secure dictation request id.",
        });
        return;
      }
      registerBrowserVoiceAnchor(
        captureId,
        { pluginId, actionId, sessionId },
        {
          expectedText: snapshot.text,
          selectionStart: snapshot.selectionStart,
          selectionEnd: snapshot.selectionEnd,
        },
      );
      uploadingAnchorRef.current = captureId;
      const abort = new AbortController();
      uploadAbortRef.current = abort;
      const result = await invokePluginBrowserVoiceInput(
        pluginId,
        capture.method,
        sessionId,
        captureId,
        audio,
        durationMs,
        {},
        abort.signal,
      );
      if (uploadAbortRef.current === abort) uploadAbortRef.current = null;
      if (uploadingAnchorRef.current === captureId) uploadingAnchorRef.current = null;
      if (requestIdRef.current !== capture.id || abort.signal.aborted) return;
      if (result.kind === "error") {
        removeBrowserVoiceAnchor(captureId);
        showError({ label: "Transcription failed", message: result.message });
        return;
      }
      onAccepted();
      setVoiceError(null);
      setElapsedMs(0);
      setVoicePhase("idle");
    },
    [actionId, getSnapshot, onAccepted, pluginId, releaseCapture, sessionId, setVoicePhase, showError],
  );

  const finishCapture = useCallback(
    (captureId: number) => {
      const capture = activeCaptureRef.current;
      if (!capture || capture.id !== captureId || capture.recorder.state === "inactive") return;
      capture.snapshot = getSnapshot();
      window.clearInterval(capture.intervalId);
      window.clearTimeout(capture.timeoutId);
      setElapsedMs(Math.min(Date.now() - capture.startedAt, BROWSER_VOICE_MAX_DURATION_MS));
      setVoicePhase("uploading");
      try {
        capture.recorder.stop();
      } catch {
        failCapture(capture.id, {
          label: "Recording failed",
          message: "The browser could not finish the microphone recording.",
        });
      }
    },
    [failCapture, getSnapshot, setVoicePhase],
  );

  useEffect(
    () => () => {
      requestIdRef.current += 1;
      uploadAbortRef.current?.abort();
      uploadAbortRef.current = null;
      if (uploadingAnchorRef.current) removeBrowserVoiceAnchor(uploadingAnchorRef.current);
      uploadingAnchorRef.current = null;
      const capture = activeCaptureRef.current;
      if (capture) releaseCapture(capture, true);
    },
    [actionId, pluginId, releaseCapture, sessionId],
  );

  const stop = () => {
    if (phaseRef.current === "starting") {
      requestIdRef.current += 1;
      setVoicePhase("idle");
      return;
    }
    const capture = activeCaptureRef.current;
    if (phaseRef.current === "recording" && capture) finishCapture(capture.id);
  };

  const start = async () => {
    if (!supported || !method || (phaseRef.current !== "idle" && phaseRef.current !== "error")) return;
    const requestId = requestIdRef.current + 1;
    requestIdRef.current = requestId;
    setVoiceError(null);
    setElapsedMs(0);
    setVoicePhase("starting");
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      if (requestIdRef.current !== requestId) {
        stopStream(stream);
        return;
      }
      let recorder: MediaRecorder;
      try {
        recorder = new MediaRecorder(stream, mediaRecorderOptions());
      } catch (error) {
        stopStream(stream);
        if (requestIdRef.current === requestId) showError(microphoneError(error));
        return;
      }
      const capture: ActiveVoiceCapture = {
        id: requestId,
        method,
        recorder,
        stream,
        chunks: [],
        bytes: 0,
        startedAt: Date.now(),
        intervalId: 0,
        timeoutId: 0,
        snapshot: null,
      };
      activeCaptureRef.current = capture;
      recorder.ondataavailable = (event) => {
        if (activeCaptureRef.current?.id !== capture.id || event.data.size === 0) return;
        capture.chunks.push(event.data);
        capture.bytes += event.data.size;
        if (capture.bytes > BROWSER_VOICE_MAX_AUDIO_BYTES) {
          failCapture(capture.id, {
            label: "Recording too large",
            message: "The recording exceeded the 8 MiB limit. Try a shorter dictation.",
          });
        }
      };
      recorder.onerror = () => {
        failCapture(capture.id, {
          label: "Recording failed",
          message: "The browser reported a microphone recording error.",
        });
      };
      recorder.onstop = () => {
        void finalizeCapture(capture);
      };
      try {
        recorder.start(1000);
      } catch {
        failCapture(capture.id, {
          label: "Recording failed",
          message: "The browser could not start microphone recording.",
        });
        return;
      }
      capture.intervalId = window.setInterval(() => {
        if (activeCaptureRef.current?.id !== capture.id) return;
        const elapsed = Date.now() - capture.startedAt;
        setElapsedMs(Math.min(elapsed, BROWSER_VOICE_MAX_DURATION_MS));
        if (elapsed >= BROWSER_VOICE_MAX_DURATION_MS) finishCapture(capture.id);
      }, 250);
      capture.timeoutId = window.setTimeout(() => finishCapture(capture.id), BROWSER_VOICE_MAX_DURATION_MS);
      setVoicePhase("recording");
    } catch (error) {
      if (requestIdRef.current === requestId) showError(microphoneError(error));
    }
  };

  const toggle = () => {
    if (!enabled) return;
    if (phaseRef.current === "recording" || phaseRef.current === "starting") {
      stop();
      return;
    }
    if (phaseRef.current === "idle" || phaseRef.current === "error") void start();
  };

  return { elapsedMs, error: voiceError, phase, supported, toggle };
}
