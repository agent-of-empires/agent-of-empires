import { useCallback, useEffect, useLayoutEffect, useRef, type RefObject } from "react";
import {
  bindHiddenInput,
  invalidateRetainedImeContext,
  registerMobileKeyboardProxyReceiver,
  resetHiddenInput,
  type MobileKeyboardProxyInput,
} from "../../lib/mobileKeyboardProxy";
import { writeClipboard } from "../../lib/clipboard";
import {
  altPrintableMetaKey,
  controlCode,
  escapePastePath,
  specialKeySequence,
  type KeyboardLayoutReader,
} from "./keySequences";

const textareaOf = (e: Event) => (e.target instanceof HTMLTextAreaElement ? e.target : null);

/** Keyboard, IME, and paste handling for the hidden terminal input and App's persistent keyboard proxy. */
export function useTerminalInput({
  active,
  inputRef,
  ctrlActiveRef,
  clearCtrl,
  sendData,
  sendPaste,
  uploadPastedImage,
}: {
  active: boolean;
  inputRef: RefObject<HTMLTextAreaElement | null>;
  ctrlActiveRef: RefObject<boolean>;
  clearCtrl: () => void;
  sendData: (data: string) => boolean;
  /** Pastes through tmux, which adds bracketed-paste markers only when the pane enabled them. */
  sendPaste: (text: string, submit: boolean) => boolean;
  uploadPastedImage: (file: File) => Promise<string | null>;
}) {
  const composingRef = useRef(false);
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

  // The return value tells the hidden textarea whether it may keep the edit.
  const handleProxyInput = useCallback(
    (input: MobileKeyboardProxyInput): boolean => {
      switch (input.inputType) {
        case "edit":
          return sendKeys("\x7f".repeat(input.deleted) + input.data);
        case "insertLineBreak":
        case "insertParagraph":
          return sendKeys("\r");
        case "insertFromPaste":
          // The paste bypasses the textarea, so the retained IME syllable no longer mirrors the line.
          invalidateRetainedImeContext(inputRef.current);
          if (input.data) sendPaste(input.data, false);
          return true;
      }
    },
    [sendKeys, sendPaste, inputRef],
  );

  useEffect(() => {
    const ta = inputRef.current;
    if (!ta) return;
    return bindHiddenInput(ta, handleProxyInput, "live");
  }, [handleProxyInput, inputRef]);

  const onKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (composingRef.current || e.isComposing) return;
      // A plain Backspace edits the textarea natively, so autorepeat and word deletes reach the pane as a diff.
      if (e.key === "Backspace" && !e.altKey && !e.ctrlKey && !e.metaKey) return;
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
        if (text) sendPaste(text, false);
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
        else resetHiddenInput(target);
        sendPaste(` ${parts.join(" ")} `, false);
      })();
    },
    [inputRef, sendPaste, uploadPastedImage],
  );

  // Composition text is sent by the hidden input's diff at compositionend; this only gates key handling.
  const onCompositionStart = useCallback(() => {
    composingRef.current = true;
  }, []);
  const onCompositionEnd = useCallback(() => {
    composingRef.current = false;
  }, []);

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
      ["compositionend", onCompositionEnd],
    ];
    for (const [type, fn, capture] of listeners) proxy.addEventListener(type, fn, capture);
    return () => {
      unregister();
      for (const [type, fn, capture] of listeners) proxy.removeEventListener(type, fn, capture);
    };
  }, [active, onKeyDownCapture, onKeyDown, handleProxyInput, onPaste, onCompositionStart, onCompositionEnd]);

  return { onKeyDown, onKeyDownCapture, onPaste, onCompositionStart, onCompositionEnd };
}
