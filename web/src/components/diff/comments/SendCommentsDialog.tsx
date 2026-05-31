import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { CommentMarkdown } from "./CommentMarkdown";
import { buildCommentsMarkdown, buildDiffCommentsPrompt } from "./buildPrompt";
import type { DiffComment } from "./types";

interface Props {
  sessionId: string;
  comments: DiffComment[];
  isMultiRepo: boolean;
  /** Same gate as the banner Send button. Reflects
   *  `cockpit_mode && cockpit_worker_state === "running"`. False
   *  disables the Send button so prompts don't sink when the worker
   *  isn't ready. */
  sendEnabled: boolean;
  sendDisabledReason?: string;
  introDraft: string;
  outroDraft: string;
  clearAfterSend: boolean;
  onChangeIntro: (v: string) => void;
  onChangeOutro: (v: string) => void;
  onChangeClearAfterSend: (v: boolean) => void;
  onClose: () => void;
  onSent: () => void;
}

/** Three-piece compose dialog: editable intro textarea, read-only
 *  preview of the assembled comments markdown, editable outro
 *  textarea. The final prompt is composed at send time so the user's
 *  intro/outro edits don't fall out of sync if comments change
 *  underneath. */
export function SendCommentsDialog({
  sessionId,
  comments,
  isMultiRepo,
  sendEnabled,
  sendDisabledReason,
  introDraft,
  outroDraft,
  clearAfterSend,
  onChangeIntro,
  onChangeOutro,
  onChangeClearAfterSend,
  onClose,
  onSent,
}: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const preview = useMemo(
    () => buildCommentsMarkdown(comments, { isMultiRepo }),
    [comments, isMultiRepo],
  );

  const send = useCallback(async () => {
    if (busy || comments.length === 0 || !sendEnabled) return;
    setBusy(true);
    setError(null);
    const built = buildDiffCommentsPrompt(comments, introDraft, outroDraft, {
      isMultiRepo,
    });
    try {
      const res = await fetch(
        `/api/sessions/${encodeURIComponent(sessionId)}/cockpit/prompt/diff-comments`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(built),
        },
      );
      if (!res.ok) {
        const text = (await res.text().catch(() => "")).slice(0, 500);
        if (mountedRef.current) {
          setError(`Failed to send (${res.status}). ${text}`.trim());
        }
        return;
      }
      onSent();
    } catch (e) {
      const message = e instanceof Error ? e.message : "Network error";
      if (mountedRef.current) {
        setError(`Failed to send: ${message}`);
      }
    } finally {
      if (mountedRef.current) {
        setBusy(false);
      }
    }
  }, [
    busy,
    comments,
    introDraft,
    outroDraft,
    isMultiRepo,
    sendEnabled,
    sessionId,
    onSent,
  ]);

  // Trap Esc/Cmd+Enter at the document level so editing in the textareas
  // doesn't intercept the dialog hotkeys. Esc is blocked while a send
  // is in flight so the user doesn't dismiss a request mid-flight.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (busy) return;
        e.preventDefault();
        onClose();
      } else if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
        e.preventDefault();
        void send();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [busy, onClose, send]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="bg-surface-900 border border-surface-700 rounded-lg shadow-xl w-full max-w-2xl max-h-[90vh] flex flex-col">
        <div className="px-4 py-3 border-b border-surface-700/60 flex items-center gap-2">
          <h2 className="text-sm font-semibold text-text-primary">
            Send diff comments
          </h2>
          <span className="text-[11px] text-text-dim">
            {comments.length} comment{comments.length === 1 ? "" : "s"}
          </span>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="ml-auto text-text-dim hover:text-text-secondary cursor-pointer"
          >
            ×
          </button>
        </div>
        <div className="flex-1 overflow-auto px-4 py-3 space-y-3">
          <section>
            <label className="block text-[11px] text-text-dim mb-1">
              Intro (optional)
            </label>
            <textarea
              value={introDraft}
              onChange={(e) => onChangeIntro(e.target.value)}
              placeholder="Anything you want to say before the comments..."
              rows={2}
              className="w-full bg-surface-950 border border-surface-700 rounded px-2 py-1.5 text-[12px] font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none resize-y"
            />
          </section>

          <section>
            <div className="text-[11px] text-text-dim mb-1">
              Comments preview (auto-generated, read-only)
            </div>
            <div className="border border-surface-700/60 rounded p-3 bg-surface-950 max-h-72 overflow-auto text-[13px]">
              {preview ? (
                <CommentMarkdown text={preview} />
              ) : (
                <span className="text-text-dim italic">No comments.</span>
              )}
            </div>
          </section>

          <section>
            <label className="block text-[11px] text-text-dim mb-1">
              Outro
            </label>
            <textarea
              value={outroDraft}
              onChange={(e) => onChangeOutro(e.target.value)}
              placeholder="Please address these comments."
              rows={2}
              className="w-full bg-surface-950 border border-surface-700 rounded px-2 py-1.5 text-[12px] font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none resize-y"
            />
          </section>

          {error && (
            <div className="text-[12px] text-status-error bg-status-error/10 rounded p-2">
              {error}
            </div>
          )}
        </div>
        <div className="px-4 py-3 border-t border-surface-700/60 flex items-center gap-3">
          <label className="flex items-center gap-1.5 text-[11px] text-text-dim cursor-pointer">
            <input
              type="checkbox"
              checked={clearAfterSend}
              onChange={(e) => onChangeClearAfterSend(e.target.checked)}
              className="cursor-pointer"
            />
            Clear comments after sending
          </label>
          <div className="ml-auto flex items-center gap-2">
            <button
              type="button"
              onClick={onClose}
              className="text-[12px] px-3 py-1.5 rounded text-text-dim hover:text-text-secondary hover:bg-surface-800 cursor-pointer transition-colors"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={() => void send()}
              disabled={busy || comments.length === 0 || !sendEnabled}
              title={sendEnabled ? undefined : sendDisabledReason}
              className="text-[12px] px-3 py-1.5 rounded bg-brand-600 text-white hover:bg-brand-500 disabled:bg-surface-700 disabled:text-text-dim disabled:cursor-not-allowed cursor-pointer transition-colors"
            >
              {busy ? "Sending..." : "Send"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
