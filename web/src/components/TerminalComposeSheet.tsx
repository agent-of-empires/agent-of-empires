import { useCallback, useLayoutEffect, useRef, useState } from "react";
import { useFocusTrap } from "../hooks/useFocusTrap";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";
import { BACKDROP, BUTTON_GHOST, BUTTON_PRIMARY, BUTTON_SECONDARY, INPUT, SHEET, SHEET_HANDLE } from "../lib/surfaces";

// Per terminal, in memory: closing keeps the draft and a delivered send clears it.
const drafts = new Map<string, string>();

interface Props {
  readOnly: boolean;
  draftKey: string;
  /** Soft keyboard height above the home-indicator inset, which the sheet adds since its parent drops it. */
  bottomInset: number;
  /** Whether the pane will receive the text. */
  onSubmit: (text: string, submit: boolean) => boolean;
  onClose: (refocusTerminal: boolean) => void;
}

/**
 * A native textarea for writing a prompt with autocorrect, dictation, and long-press paste, delivered as one paste.
 * It is uncontrolled so a React re-render never rewrites text iOS dictation is still inserting.
 */
export function TerminalComposeSheet({ draftKey, bottomInset, readOnly, onSubmit, onClose }: Props) {
  const textRef = useRef<HTMLTextAreaElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const [empty, setEmpty] = useState(() => !drafts.get(draftKey)?.trim());

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
    if (readOnly || !text.trim() || !onSubmit(text, submit)) return;
    drafts.delete(draftKey);
    // A tapped Send drops the keyboard; Insert and the hardware chord keep typing in the terminal.
    if (!keepTyping) textRef.current?.blur();
    onClose(keepTyping);
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
          defaultValue={drafts.get(draftKey) ?? ""}
          placeholder="Type, dictate, or long-press to paste"
          rows={2}
          onInput={(e) => {
            const value = e.currentTarget.value;
            drafts.set(draftKey, value);
            setEmpty(!value.trim());
            grow();
          }}
          // 16px stops iOS from zooming the page on focus.
          style={{ fontSize: "16px" }}
          className={`${INPUT} max-h-[40vh] resize-none overflow-y-auto`}
        />
        {/* preventDefault keeps the keyboard up until the terminal input takes focus. */}
        <div className="flex items-center gap-2" onMouseDown={(e) => e.preventDefault()}>
          <button type="button" className={BUTTON_GHOST} onClick={() => onClose(false)}>
            Close
          </button>
          <span className="flex-1" />
          <button
            type="button"
            disabled={readOnly || empty}
            className={BUTTON_SECONDARY}
            onClick={() => deliver(false)}
          >
            Insert
          </button>
          <button type="button" disabled={readOnly || empty} className={BUTTON_PRIMARY} onClick={() => deliver(true)}>
            Send
          </button>
        </div>
      </div>
    </div>
  );
}
