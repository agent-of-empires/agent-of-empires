import { useState } from "react";
import { CommentMarkdown } from "./CommentMarkdown";
import { CommentForm } from "./CommentForm";
import type { AnchoredComment } from "./types";

interface Props {
  anchored: AnchoredComment;
  onSave: (id: string, body: string) => void;
  onDelete: (id: string) => void;
}

/** Saved-comment view. Body is rendered as markdown via the existing
 *  structured view Markdown component. Switching to edit mode reuses
 *  CommentForm. Stale comments show a `[stale]` chip; they remain
 *  editable so the user can rewrite or delete them. */
export function CommentCard({ anchored, onSave, onDelete }: Props) {
  const [editing, setEditing] = useState(false);
  const { comment, status } = anchored;

  if (editing) {
    return (
      <CommentForm
        startLine={comment.startLine}
        endLine={comment.endLine}
        side={comment.side}
        initialBody={comment.body}
        onSave={(body) => {
          onSave(comment.id, body);
          setEditing(false);
        }}
        onCancel={() => setEditing(false)}
      />
    );
  }

  const range =
    comment.startLine === comment.endLine
      ? `line ${comment.startLine}`
      : `lines ${comment.startLine}-${comment.endLine}`;

  return (
    <div className="border-y border-surface-700/40 bg-surface-850 px-3 py-2">
      <div className="flex items-center gap-2 mb-1.5 text-[11px] font-mono">
        <span className="text-text-dim">
          {range} ({comment.side})
        </span>
        {status === "stale" && (
          <span
            className="px-1 rounded bg-status-error/15 text-status-error"
            title="The line range no longer appears in the current diff. The comment will still be sent to the agent using the original snippet."
          >
            stale
          </span>
        )}
        <div className="ml-auto flex items-center gap-1">
          <button
            type="button"
            onClick={() => setEditing(true)}
            className="text-[10px] px-1.5 py-0.5 rounded text-text-dim hover:text-text-secondary hover:bg-surface-800 cursor-pointer transition-colors"
          >
            Edit
          </button>
          <button
            type="button"
            onClick={() => onDelete(comment.id)}
            className="text-[10px] px-1.5 py-0.5 rounded text-text-dim hover:text-status-error hover:bg-surface-800 cursor-pointer transition-colors"
          >
            Delete
          </button>
        </div>
      </div>
      <div className="text-[13px] text-text-primary">
        <CommentMarkdown text={comment.body} />
      </div>
    </div>
  );
}
