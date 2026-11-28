import { useEffect, useRef, useState } from "react";
import { INPUT } from "../../../lib/surfaces";
import { BUTTON_GHOST_SM, BUTTON_PRIMARY_SM } from "../../../lib/surfaces";
import type { DiffSide } from "./types";

interface Props {
  startLine: number;
  endLine: number;
  side: DiffSide;
  initialBody?: string;
  onSave: (body: string) => void;
  onCancel: () => void;
}

/** Inline composer; Cmd/Ctrl+Enter saves, Esc cancels, empty bodies cannot be saved. */
export function CommentForm({ startLine, endLine, side, initialBody = "", onSave, onCancel }: Props) {
  const [body, setBody] = useState(initialBody);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const trimmed = body.trim();
  const canSave = trimmed.length > 0;

  const range = startLine === endLine ? `line ${startLine}` : `lines ${startLine}-${endLine}`;

  return (
    <div className="border-y border-brand-600/30 bg-surface-850 px-3 py-2">
      <div className="text-[12px] text-text-dim mb-1.5 font-mono">
        Commenting on {range} ({side})
      </div>
      <textarea
        ref={textareaRef}
        value={body}
        onChange={(e) => setBody(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onCancel();
          } else if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
            e.preventDefault();
            if (canSave) onSave(trimmed);
          }
        }}
        placeholder="Leave a comment (markdown supported). Cmd/Ctrl+Enter to save, Esc to cancel."
        rows={3}
        className={`${INPUT} font-mono resize-y`}
      />
      <div className="mt-2 flex items-center gap-1.5 justify-end">
        <button type="button" onClick={onCancel} className={BUTTON_GHOST_SM}>
          Cancel
        </button>
        <button
          type="button"
          onClick={() => canSave && onSave(trimmed)}
          disabled={!canSave}
          className={BUTTON_PRIMARY_SM}
        >
          Save
        </button>
      </div>
    </div>
  );
}
