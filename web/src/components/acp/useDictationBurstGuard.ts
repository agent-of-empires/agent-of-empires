// iOS Safari dictation guard for the composer. WebKit sends each partial result as
// `insertReplacementText` and tracks a private range into the textarea; any
// controlled-value write invalidates it and partials start duplicating. During a
// burst the guard suppresses assistant-ui's flush, buffers the value, and drains
// it into `setText` once the burst ends (timeout, blur, or other input).

import { useEffect, useRef } from "react";

export type DictationBurstState = { active: false } | { active: true; sinceMs: number };

export type DictationEvent =
  | { kind: "input"; inputType: string; nowMs: number }
  | { kind: "timeout"; nowMs: number }
  | { kind: "blur" };

export interface DictationDecision {
  next: DictationBurstState;
  /** preventDefault the React onChange so the primitive skips its setText flush. */
  suppressUpstreamChange: boolean;
  /** Flush the buffered value; set only when an active burst ends. */
  flushPending: boolean;
  /** (Re)arm the burst timeout, when entering or extending a burst. */
  armTimeoutMs: number | null;
}

/** Spans a breath pause between phrases without visible lag after the mic stops. */
export const DICTATION_BURST_TIMEOUT_MS = 1200;

export function decideDictationAction(prev: DictationBurstState, ev: DictationEvent): DictationDecision {
  if (ev.kind === "input" && ev.inputType === "insertReplacementText") {
    return {
      next: { active: true, sinceMs: ev.nowMs },
      suppressUpstreamChange: true,
      flushPending: false,
      armTimeoutMs: DICTATION_BURST_TIMEOUT_MS,
    };
  }
  return { next: { active: false }, suppressUpstreamChange: false, flushPending: prev.active, armTimeoutMs: null };
}

export interface DictationGuard {
  /** From onBeforeInput, before the primitive's onChange runs. */
  observeInputType: (inputType: string, nowMs: number) => void;
  /** From onChange; true means preventDefault the event. */
  shouldSuppressUpstream: (value: string) => boolean;
  /** From onBlur, so the buffer lands before a Send click reads the text. */
  flushOnBlur: () => void;
}

export function useDictationBurstGuard(setText: (text: string) => void): DictationGuard {
  const stateRef = useRef<DictationBurstState>({ active: false });
  const bufferRef = useRef<string | null>(null);
  const timerRef = useRef<number | null>(null);

  const clearTimer = () => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };

  const flush = () => {
    const buffered = bufferRef.current;
    stateRef.current = { active: false };
    bufferRef.current = null;
    clearTimer();
    if (buffered !== null) setText(buffered);
  };

  useEffect(
    () => () => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
    },
    [],
  );

  return {
    observeInputType(inputType, nowMs) {
      const decision = decideDictationAction(stateRef.current, { kind: "input", inputType, nowMs });
      if (decision.flushPending) flush();
      stateRef.current = decision.next;
      if (decision.armTimeoutMs !== null) {
        clearTimer();
        timerRef.current = window.setTimeout(flush, decision.armTimeoutMs);
      }
    },
    shouldSuppressUpstream(value) {
      if (!stateRef.current.active) return false;
      bufferRef.current = value;
      return true;
    },
    flushOnBlur() {
      if (decideDictationAction(stateRef.current, { kind: "blur" }).flushPending) flush();
    },
  };
}
