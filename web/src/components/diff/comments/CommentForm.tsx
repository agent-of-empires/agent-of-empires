import { useEffect, useRef, useState } from "react";
import type { DiffSide } from "./types";

interface Props {
  startLine: number;
  endLine: number;
  side: DiffSide;
  initialBody?: string;
  onSave: (body: string) => void;
  onCancel: () => void;
}

/** Inline composer rendered beneath the last row of the selected range.
 *  Cmd/Ctrl+Enter saves; Esc cancels. Empty bodies are rejected (the
 *  Save button stays disabled). */
export function CommentForm({
  startLine,
  endLine,
  side,
  initialBody = "",
  onSave,
  onCancel,
}: Props) {
  const [body, setBody] = useState(initialBody);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const trimmed = body.trim();
  const canSave = trimmed.length > 0;

  const range =
    startLine === endLine
      ? `line ${startLine}`
      : `lines ${startLine}-${endLine}`;

  return (
    <div className="border-y border-brand-600/30 bg-surface-850 px-3 py-2">
      <div className="text-[11px] text-text-dim mb-1.5 font-mono">
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
        className="w-full bg-surface-900 border border-surface-700 rounded px-2 py-1.5 text-[12px] font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none resize-y"
      />
      <div className="mt-2 flex items-center gap-2 justify-end">
        <button
          type="button"
          onClick={onCancel}
          className="text-[11px] px-2 py-1 rounded text-text-dim hover:text-text-secondary hover:bg-surface-800 cursor-pointer transition-colors"
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={() => canSave && onSave(trimmed)}
          disabled={!canSave}
          className="text-[11px] px-2 py-1 rounded bg-brand-600 text-white hover:bg-brand-500 disabled:bg-surface-700 disabled:text-text-dim disabled:cursor-not-allowed cursor-pointer transition-colors"
        >
          Save
        </button>
      </div>
    </div>
  );
}
