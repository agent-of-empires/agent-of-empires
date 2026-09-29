import { useCallback, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";

// Per terminal, in memory: closing keeps the draft and a delivered send clears it.
const drafts = new Map<string, string>();

interface Props {
  draftKey: string;
  /** Height the soft keyboard covers, so the sheet sits above it. */
  bottomInset: number;
  /** Whether the pane will receive the text. */
  onSubmit: (text: string, submit: boolean) => boolean;
  onClose: (refocusTerminal: boolean) => void;
}

/**
 * A native textarea for writing a prompt with autocorrect, dictation, and long-press paste, delivered as one paste.
 * It is uncontrolled so a React re-render never rewrites text iOS dictation is still inserting.
 */
export function TerminalComposeSheet({ draftKey, bottomInset, onSubmit, onClose }: Props) {
  const textRef = useRef<HTMLTextAreaElement>(null);
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

  const deliver = (submit: boolean) => {
    const text = textRef.current?.value ?? "";
    if (!text.trim() || !onSubmit(text, submit)) return;
    drafts.delete(draftKey);
    onClose(true);
  };

  const onKeyDown = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose(false);
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      deliver(true);
    }
  };

  const action = "h-10 px-3 rounded-md text-sm transition-colors disabled:opacity-40";

  return (
    <div className="absolute inset-x-0 top-0 z-30 flex flex-col justify-end" style={{ bottom: bottomInset }}>
      <div className="absolute inset-0 bg-black/40" onClick={() => onClose(false)} data-compose-backdrop />
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Compose"
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
          onKeyDown={onKeyDown}
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
            disabled={empty}
            className={`${action} border border-surface-700 text-text-primary active:bg-surface-800`}
            onClick={() => deliver(false)}
          >
            Insert
          </button>
          <button
            type="button"
            disabled={empty}
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
