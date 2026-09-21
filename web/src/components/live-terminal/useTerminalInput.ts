import { useCallback, useEffect, useLayoutEffect, useRef, type RefObject } from "react";
import {
  forwardTerminalBeforeInput,
  invalidateRetainedImeContext,
  registerMobileKeyboardProxyReceiver,
  type MobileKeyboardProxyInput,
} from "../../lib/mobileKeyboardProxy";
import { bracketedPaste, writeClipboard } from "../../lib/clipboard";
import {
  altPrintableMetaKey,
  controlCode,
  dropLastCodePoint,
  escapePastePath,
  plainRunAfter,
  specialKeySequence,
  type KeyboardLayoutReader,
} from "./keySequences";

const textareaOf = (e: Event) => (e.target instanceof HTMLTextAreaElement ? e.target : null);

/** Keyboard, IME, and paste handling for the hidden terminal input and App's persistent keyboard proxy. */
export function useTerminalInput({
  active,
  inputRef,
  typedWordRef,
  ctrlActiveRef,
  clearCtrl,
  sendData,
  uploadPastedImage,
}: {
  active: boolean;
  inputRef: RefObject<HTMLTextAreaElement | null>;
  typedWordRef: RefObject<string>;
  ctrlActiveRef: RefObject<boolean>;
  clearCtrl: () => void;
  sendData: (data: string) => boolean;
  uploadPastedImage: (file: File) => Promise<string | null>;
}) {
  const composingRef = useRef(false);
  // Whether the composition in flight took over already-typed text; null until its first update.
  const retroactiveRef = useRef<boolean | null>(null);
  const activeRef = useRef(active);
  useLayoutEffect(() => {
    activeRef.current = active;
  }, [active]);

  const keyboardLayoutRef = useRef<KeyboardLayoutReader | null>(null);
  useEffect(() => {
    const keyboard = (navigator as Navigator & { keyboard?: { getLayoutMap?: () => Promise<KeyboardLayoutReader> } })
      .keyboard;
    let cancelled = false;
    keyboard
      ?.getLayoutMap?.()
      .then((layoutMap) => {
        if (!cancelled) keyboardLayoutRef.current = layoutMap;
      })
      // Firefox and Safari have no layout map; the physical-key fallback applies.
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  /** Whether `data` itself reached the pane; a virtual-Ctrl chord sends a control code instead. */
  const sendKeys = useCallback(
    (data: string) => {
      const ctrl = ctrlActiveRef.current ? controlCode(data) : null;
      if (ctrl == null) return sendData(data);
      sendData(ctrl);
      clearCtrl();
      return false;
    },
    [sendData, ctrlActiveRef, clearCtrl],
  );

  // Native beforeinput: React's synthetic one carries no inputType in Chromium.
  // The return value tells the shadow textarea whether it may keep the edit.
  const handleProxyInput = useCallback(
    (input: MobileKeyboardProxyInput): boolean => {
      if (composingRef.current || input.isComposing) return true;
      const run = typedWordRef.current;
      typedWordRef.current = "";
      switch (input.inputType) {
        case "insertText": {
          const data = input.data ?? "";
          if (data && !sendKeys(data)) return false;
          typedWordRef.current = plainRunAfter(run, data);
          return true;
        }
        case "insertLineBreak":
        case "insertParagraph":
          return sendKeys("\r");
        case "deleteContentBackward":
          if (!sendKeys("\x7f")) return false;
          typedWordRef.current = dropLastCodePoint(run);
          return true;
        case "insertFromPaste":
          // The paste bypasses the textarea, so the retained IME syllable no longer mirrors the line.
          invalidateRetainedImeContext(inputRef.current);
          if (input.data) sendData(bracketedPaste(input.data));
          return true;
        default:
          return true;
      }
    },
    [sendKeys, sendData, typedWordRef, inputRef],
  );

  useEffect(() => {
    const ta = inputRef.current;
    if (!ta) return;
    const onBeforeInput = (ev: InputEvent) => forwardTerminalBeforeInput(ev, handleProxyInput);
    ta.addEventListener("beforeinput", onBeforeInput);
    return () => ta.removeEventListener("beforeinput", onBeforeInput);
  }, [handleProxyInput, inputRef]);

  const onKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (composingRef.current || e.isComposing) return;
      const seq = specialKeySequence(e);
      if (seq) {
        e.preventDefault();
        // Enter submits and other special keys rewrite the line, so the IME shadow is stale either way.
        invalidateRetainedImeContext(textareaOf(e));
        sendData(seq);
        return;
      }
      // Ctrl+Shift+C copies the rendered selection; the focused textarea has nothing to copy.
      if (e.ctrlKey && e.shiftKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === "c") {
        e.preventDefault();
        const text = window.getSelection()?.toString() ?? "";
        if (text) void writeClipboard(text);
        return;
      }
      // Hardware Ctrl+letter chords, except Ctrl+V, which stays the native paste.
      const ctrl = e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() !== "v" ? controlCode(e.key) : null;
      if (ctrl) {
        e.preventDefault();
        invalidateRetainedImeContext(textareaOf(e));
        sendData(ctrl);
      }
    },
    [sendData],
  );

  // Capture phase, so printable Alt chords beat browser accelerators such as Alt+V.
  const onKeyDownCapture = useCallback(
    (e: KeyboardEvent) => {
      if (composingRef.current || e.isComposing || !e.altKey || e.ctrlKey || e.metaKey) return;
      const metaKey = altPrintableMetaKey(e, keyboardLayoutRef.current);
      if (!metaKey) return;
      e.preventDefault();
      e.stopPropagation();
      invalidateRetainedImeContext(textareaOf(e));
      sendData(`\x1b${metaKey}`);
    },
    [sendData],
  );

  const onPaste = useCallback(
    (e: ClipboardEvent) => {
      // clipboardData may not survive an await, so read it synchronously.
      const text = e.clipboardData?.getData("text/plain") ?? "";
      const imageFiles = Array.from(e.clipboardData?.items ?? [])
        .filter((it) => it.kind === "file")
        .map((it) => it.getAsFile())
        .filter((f): f is File => f != null && f.type.startsWith("image/"));
      e.preventDefault();
      invalidateRetainedImeContext(textareaOf(e));
      if (imageFiles.length === 0) {
        if (text) sendData(bracketedPaste(text));
        return;
      }
      // Images cannot be typed into the pane: upload them and paste the paths the agent can read.
      void (async () => {
        const paths = (await Promise.all(imageFiles.map((f) => uploadPastedImage(f)))).filter(
          (p): p is string => p != null,
        );
        const parts = [text.trim(), ...paths.map(escapePastePath)].filter((s) => s.length > 0);
        const target = inputRef.current;
        if (parts.length === 0 || !target) return;
        // A background session may finish its paste but must not invalidate the foreground proxy.
        if (activeRef.current) invalidateRetainedImeContext(target);
        else target.value = "";
        sendData(bracketedPaste(` ${parts.join(" ")} `));
      })();
    },
    [inputRef, sendData, uploadPastedImage],
  );

  const onCompositionStart = useCallback(() => {
    composingRef.current = true;
    retroactiveRef.current = null;
  }, []);
  // A retroactive composition (SwiftKey adopting the typed word) carries the whole run in its first update.
  const onCompositionUpdate = useCallback(
    (e: CompositionEvent) => {
      if (retroactiveRef.current !== null) return;
      const run = typedWordRef.current;
      retroactiveRef.current = run !== "" && (e.data ?? "").startsWith(run);
    },
    [typedWordRef],
  );
  const onCompositionEnd = useCallback(
    (e: CompositionEvent) => {
      composingRef.current = false;
      const retroactive = retroactiveRef.current === true;
      retroactiveRef.current = null;
      const run = typedWordRef.current;
      typedWordRef.current = "";
      const data = e.data ?? "";
      // Only a composition that adopted the typed word may drop that prefix.
      const rest = retroactive && data.startsWith(run) ? data.slice(run.length) : data;
      if (!rest) typedWordRef.current = run;
      else if (!sendKeys(rest)) invalidateRetainedImeContext(textareaOf(e));
      else if (retroactive) typedWordRef.current = plainRunAfter(run, rest);
    },
    [sendKeys, typedWordRef],
  );

  // App's persistent keyboard proxy keeps focus after a session tap on iOS, so its native events are handled directly.
  useEffect(() => {
    if (!active) return;
    const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
    if (!proxy) return;
    const unregister = registerMobileKeyboardProxyReceiver(handleProxyInput);
    const listeners: [string, EventListener, boolean?][] = [
      ["keydown", onKeyDownCapture as EventListener, true],
      ["keydown", onKeyDown as EventListener],
      ["paste", onPaste as EventListener],
      ["compositionstart", onCompositionStart],
      ["compositionupdate", onCompositionUpdate as EventListener],
      ["compositionend", onCompositionEnd as EventListener],
    ];
    for (const [type, fn, capture] of listeners) proxy.addEventListener(type, fn, capture);
    return () => {
      unregister();
      for (const [type, fn, capture] of listeners) proxy.removeEventListener(type, fn, capture);
    };
  }, [
    active,
    onKeyDownCapture,
    onKeyDown,
    handleProxyInput,
    onPaste,
    onCompositionStart,
    onCompositionUpdate,
    onCompositionEnd,
  ]);

  return { onKeyDown, onKeyDownCapture, onPaste, onCompositionStart, onCompositionUpdate, onCompositionEnd };
}
