import { useCallback, useLayoutEffect, useRef, useState } from "react";
import { useFocusTrap } from "../hooks/useFocusTrap";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";
import { BACKDROP, BUTTON_GHOST, BUTTON_PRIMARY, BUTTON_SECONDARY, INPUT, SHEET, SHEET_HANDLE } from "../lib/surfaces";

// Per terminal, in memory: closing keeps the draft and a delivered send clears it.
const drafts = new Map<string, string>();

interface Props {
  readOnly: boolean;
  draftKey: string;
  /** Explicit recovery composition starts empty and never loads or overwrites a saved draft. */
  freshDraft?: boolean;
  /** Soft keyboard height above the home-indicator inset, which the sheet adds since its parent drops it. */
  bottomInset: number;
  /** Whether the pane will receive the text. */
  onSubmit: (text: string, submit: boolean) => boolean | Promise<boolean>;
  onClose: (refocusTerminal: boolean) => void;
}

/**
 * A native textarea for writing a prompt with autocorrect, dictation, and long-press paste, delivered as one paste.
 * It is uncontrolled so a React re-render never rewrites text iOS dictation is still inserting.
 */
export function TerminalComposeSheet({
  draftKey,
  bottomInset,
  readOnly,
  freshDraft = false,
  onSubmit,
  onClose,
}: Props) {
  const textRef = useRef<HTMLTextAreaElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const [empty, setEmpty] = useState(() => freshDraft || !drafts.get(draftKey)?.trim());
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pendingRef = useRef(false);
  const mountedRef = useRef(true);
  useLayoutEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const grow = useCallback(() => {
    const ta = textRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    ta.style.height = `${ta.scrollHeight}px`;
  }, []);

  // The terminal controls behind the sheet stay mounted, so Tab wraps instead of leaving.
  useFocusTrap(() => dialogRef.current);

  // With the opener's flushSync this runs inside the tap, so iOS raises the keyboard.
  useLayoutEffect(() => {
    const ta = textRef.current;
    if (!ta) return;
    grow();
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
  }, [grow]);

  const deliver = (submit: boolean, keepTyping = !submit) => {
    const text = textRef.current?.value ?? "";
    if (readOnly || pendingRef.current || !text.trim() || (freshDraft && !submit)) return;
    const accepted = () => {
      if (!mountedRef.current) return;
      if (!freshDraft) drafts.delete(draftKey);
      if (!keepTyping) textRef.current?.blur();
      onClose(keepTyping);
    };
    setError(null);
    let result: boolean | Promise<boolean>;
    try {
      result = onSubmit(text, submit);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "The operation could not be sent.");
      return;
    }
    if (typeof result === "boolean") {
      if (result) accepted();
      return;
    }
    pendingRef.current = true;
    setPending(true);
    void result
      .then((ok) => {
        if (ok) accepted();
        else if (mountedRef.current) setError("The operation was refused. Your draft is retained.");
      })
      .catch((cause: unknown) => {
        if (mountedRef.current) setError(cause instanceof Error ? cause.message : "The operation could not be sent.");
      })
      .finally(() => {
        pendingRef.current = false;
        if (mountedRef.current) setPending(false);
      });
  };

  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose(false);
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      deliver(true, true);
    }
  };

  return (
    <div
      className="absolute inset-x-0 top-0 z-30 flex flex-col justify-end"
      style={{ bottom: `calc(${bottomInset}px + env(safe-area-inset-bottom))` }}
    >
      <div className={`absolute inset-0 ${BACKDROP}`} onClick={() => onClose(false)} data-compose-backdrop />
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label="Compose"
        onKeyDown={onKeyDown}
        className={`relative flex flex-col gap-2 px-3 pb-3 ${SHEET}`}
      >
        <div className={SHEET_HANDLE} aria-hidden />
        <textarea
          ref={textRef}
          aria-label="Message"
          defaultValue={freshDraft ? "" : (drafts.get(draftKey) ?? "")}
          placeholder="Type, dictate, or long-press to paste"
          disabled={pending}
          rows={2}
          onInput={(e) => {
            const value = e.currentTarget.value;
            if (!freshDraft) drafts.set(draftKey, value);
            setEmpty(!value.trim());
            grow();
          }}
          // 16px stops iOS from zooming the page on focus.
          style={{ fontSize: "16px" }}
          className={`${INPUT} max-h-[40vh] resize-none overflow-y-auto`}
        />
        {error && (
          <div role="alert" className="text-status-error">
            {error}
          </div>
        )}
        {/* preventDefault keeps the keyboard up until the terminal input takes focus. */}
        <div className="flex items-center gap-2" onMouseDown={(e) => e.preventDefault()}>
          <button type="button" className={BUTTON_GHOST} onClick={() => onClose(false)}>
            Close
          </button>
          <span className="flex-1" />
          {!freshDraft && (
            <button
              type="button"
              disabled={readOnly || empty || pending}
              className={BUTTON_SECONDARY}
              onClick={() => deliver(false)}
            >
              Insert
            </button>
          )}
          <button
            type="button"
            disabled={readOnly || empty || pending}
            className={BUTTON_PRIMARY}
            onClick={() => deliver(true)}
          >
            Send
          </button>
        </div>
      </div>
    </div>
  );
}
