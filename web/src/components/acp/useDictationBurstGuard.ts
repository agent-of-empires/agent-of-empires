// Hook + pure state machine for the iOS-Safari native-mic dictation
// path on the structured view composer (#1431).
//
// WebKit fires `beforeinput` / `input` with `inputType:
// "insertReplacementText"` per partial recognition. It tracks a
// private NSAttributedString range pointer into the textarea's text
// storage so the next partial replaces the prior one; any JS write to
// `textarea.value` via the property setter invalidates that pointer,
// after which the next partial appends instead of replacing.
// assistant-ui's `ComposerPrimitive.Input` is fully controlled and
// re-renders on every `setText`, so without this guard the
// controlled-value reconciler re-writes the DOM value on every partial
// and dictation duplicates.
//
// The guard detects the burst via the `insertReplacementText`
// inputType, suspends the assistant-ui flush for its duration, buffers
// the textarea value in a ref, and drains the buffer into `setText`
// once when the burst ends (1200 ms timeout, blur, or non-replacement
// input event).

import { useEffect, useRef } from "react";

/** Dictation burst state for the iOS-Safari native-mic input path
 *  (#1431). Tracks whether we are currently inside a sequence of
 *  `inputType: "insertReplacementText"` events. */
export type DictationBurstState =
  | { active: false }
  | { active: true; sinceMs: number };

/** Event the composer feeds into {@link decideDictationAction}. The
 *  helper only needs the discriminator + the current monotonic clock
 *  reading; the imperative wiring lives in the hook below. */
export type DictationEvent =
  | { kind: "input"; inputType: string; nowMs: number }
  | { kind: "timeout"; nowMs: number }
  | { kind: "blur" };

/** Decision returned by {@link decideDictationAction}. The hook
 *  applies the actions imperatively (refs, timers, `setText`); the
 *  helper itself is pure so the burst-state matrix can be tested
 *  without mounting the composer + the WebKit dictation engine. */
export interface DictationDecision {
  /** Next burst state to commit. */
  next: DictationBurstState;
  /** Caller should `preventDefault()` on the React `onChange` so that
   *  radix's `composeEventHandlers` skips assistant-ui's downstream
   *  `setText` flush, which would otherwise re-render the textarea's
   *  controlled `value` prop and invalidate WebKit's dictation range
   *  pointer. */
  suppressUpstreamChange: boolean;
  /** Caller should flush the buffered textarea value into `setText`
   *  before clearing the burst. Set only on the transition from
   *  `active` to inactive. */
  flushPending: boolean;
  /** Caller should (re)arm the burst timeout for this many ms from
   *  now. Set only when entering or extending a burst. */
  armTimeoutMs: number | null;
}

/** Window (ms) after the last `insertReplacementText` event before we
 *  consider an iOS dictation burst finished and flush the buffered
 *  text into assistant-ui state. Long enough to span a breath pause
 *  between phrases in continuous dictation, short enough that the
 *  user does not perceive lag between releasing the mic and the
 *  composer state catching up. */
export const DICTATION_BURST_TIMEOUT_MS = 1200;

/** Pure decision helper for the iOS-dictation burst state machine
 *  (#1431). The hook below mirrors this state in refs and applies the
 *  returned actions; tests exercise the matrix directly. */
export function decideDictationAction(
  prev: DictationBurstState,
  ev: DictationEvent,
): DictationDecision {
  if (ev.kind === "blur") {
    if (!prev.active) {
      return {
        next: { active: false },
        suppressUpstreamChange: false,
        flushPending: false,
        armTimeoutMs: null,
      };
    }
    return {
      next: { active: false },
      suppressUpstreamChange: false,
      flushPending: true,
      armTimeoutMs: null,
    };
  }
  if (ev.kind === "timeout") {
    if (!prev.active) {
      return {
        next: { active: false },
        suppressUpstreamChange: false,
        flushPending: false,
        armTimeoutMs: null,
      };
    }
    return {
      next: { active: false },
      suppressUpstreamChange: false,
      flushPending: true,
      armTimeoutMs: null,
    };
  }
  if (ev.inputType === "insertReplacementText") {
    return {
      next: { active: true, sinceMs: ev.nowMs },
      suppressUpstreamChange: true,
      flushPending: false,
      armTimeoutMs: DICTATION_BURST_TIMEOUT_MS,
    };
  }
  if (prev.active) {
    return {
      next: { active: false },
      suppressUpstreamChange: false,
      flushPending: true,
      armTimeoutMs: null,
    };
  }
  return {
    next: { active: false },
    suppressUpstreamChange: false,
    flushPending: false,
    armTimeoutMs: null,
  };
}

/** Imperative side of the dictation-burst guard. The component calls
 *  the returned handlers from `onBeforeInput`, `onChange`, and
 *  `onBlur`; the hook owns the burst state ref, the buffered text
 *  ref, and the burst-timeout timer. */
export interface DictationGuard {
  /** Call from `onBeforeInput` with the native input event's
   *  `inputType` and the current clock reading. Updates the burst
   *  state, (re)arms or clears the burst timer, and flushes the
   *  buffered text into `setText` if the burst is ending due to a
   *  non-replacement input event. */
  observeInputType: (inputType: string, nowMs: number) => void;
  /** Call from `onChange` with the latest textarea value. Returns
   *  `true` when the caller should `preventDefault()` on the
   *  SyntheticEvent so that radix's `composeEventHandlers` skips the
   *  downstream assistant-ui flush. */
  shouldSuppressUpstream: (value: string) => boolean;
  /** Call from `onBlur`. Flushes any pending burst into `setText`
   *  before the focus shift can reach a Send-button click handler
   *  that reads `composerRuntime.getState().text`. */
  flushOnBlur: () => void;
}

/** Builds a {@link DictationGuard} bound to a single `setText` sink.
 *  The hook owns three refs (state, buffered text, timer handle) and
 *  one cleanup effect that clears the timer on unmount. */
export function useDictationBurstGuard(
  setText: (text: string) => void,
): DictationGuard {
  const stateRef = useRef<DictationBurstState>({ active: false });
  const bufferRef = useRef<string | null>(null);
  const timerRef = useRef<number | null>(null);

  const flush = () => {
    const buffered = bufferRef.current;
    stateRef.current = { active: false };
    bufferRef.current = null;
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    if (buffered !== null) {
      setText(buffered);
    }
  };

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, []);

  return {
    observeInputType(inputType, nowMs) {
      const decision = decideDictationAction(stateRef.current, {
        kind: "input",
        inputType,
        nowMs,
      });
      if (decision.flushPending) {
        flush();
      }
      stateRef.current = decision.next;
      if (decision.armTimeoutMs !== null) {
        if (timerRef.current !== null) {
          window.clearTimeout(timerRef.current);
        }
        timerRef.current = window.setTimeout(flush, decision.armTimeoutMs);
      }
    },
    shouldSuppressUpstream(value) {
      if (stateRef.current.active) {
        bufferRef.current = value;
        return true;
      }
      return false;
    },
    flushOnBlur() {
      const decision = decideDictationAction(stateRef.current, {
        kind: "blur",
      });
      if (decision.flushPending) {
        flush();
      }
    },
  };
}
