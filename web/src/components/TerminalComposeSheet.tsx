import { useCallback, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";

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
    } else if (e.key === "Tab") {
      // The terminal controls behind the sheet stay mounted, so wrap instead of leaving.
      const items = dialogRef.current?.querySelectorAll<HTMLElement>("textarea, button:not([disabled])") ?? [];
      const first = items[0];
      const last = items[items.length - 1];
      const wrapTo = e.shiftKey ? document.activeElement === first && last : document.activeElement === last && first;
      if (wrapTo) {
        e.preventDefault();
        wrapTo.focus();
      }
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      deliver(true, true);
    }
  };

  const action = "h-10 px-3 rounded-md text-sm transition-colors disabled:opacity-40";

  return (
    <div
      className="absolute inset-x-0 top-0 z-30 flex flex-col justify-end"
      style={{ bottom: `calc(${bottomInset}px + env(safe-area-inset-bottom))` }}
    >
      <div className="absolute inset-0 bg-black/40" onClick={() => onClose(false)} data-compose-backdrop />
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label="Compose"
        onKeyDown={onKeyDown}
        className="relative flex flex-col gap-2 p-2 bg-surface-900 border-t border-surface-700/40 rounded-t-lg"
      >
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
          className="w-full max-h-[40vh] resize-none overflow-y-auto bg-surface-950 border border-surface-700 rounded-md px-2 py-1.5 text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
        />
        {/* preventDefault keeps the keyboard up until the terminal input takes focus. */}
        <div className="flex items-center gap-2" onMouseDown={(e) => e.preventDefault()}>
          <button type="button" className={`${action} text-text-secondary`} onClick={() => onClose(false)}>
            Close
          </button>
          <span className="flex-1" />
          <button
            type="button"
            disabled={readOnly || empty}
            className={`${action} border border-surface-700 text-text-primary active:bg-surface-800`}
            onClick={() => deliver(false)}
          >
            Insert
          </button>
          <button
            type="button"
            disabled={readOnly || empty}
            className={`${action} px-4 font-semibold bg-brand-600 text-white active:bg-brand-700`}
            onClick={() => deliver(true)}
          >
            Send
          </button>
        </div>
      </div>
    </div>
  );
}
