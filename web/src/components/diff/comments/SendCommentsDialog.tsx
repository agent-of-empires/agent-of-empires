import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Tooltip } from "../../Tooltip";
import { CommentMarkdown } from "./CommentMarkdown";
import { buildCommentsMarkdown, buildDiffCommentsPrompt } from "./buildPrompt";
import type { DiffComment } from "./types";
import { reportTelemetrySeen } from "../../../lib/api";
import { hasFinePointer } from "../../../lib/platform";
import { BUTTON_PRIMARY, INPUT } from "../../../lib/surfaces";
import { CancelButton, CloseButton, Dialog } from "../../Dialog";
import { FIELD_LABEL } from "../../../lib/surfaces";

const LABEL = `${FIELD_LABEL} !mb-1`;
const TEXTAREA = `${INPUT} font-mono resize-y`;

interface Props {
  sessionId: string;
  delivery: "structured" | "terminal";
  comments: DiffComment[];
  isMultiRepo: boolean;
  /** False when the session cannot accept a diff-comment prompt. */
  sendEnabled: boolean;
  /** Cause plus remedy. */
  sendDisabledReason: string;
  introDraft: string;
  outroDraft: string;
  clearAfterSend: boolean;
  onChangeIntro: (v: string) => void;
  onChangeOutro: (v: string) => void;
  onChangeClearAfterSend: (v: boolean) => void;
  onClose: () => void;
  onSent: () => void;
}

/** Intro, read-only comments preview, and outro; the prompt is built at send time. */
export function SendCommentsDialog({
  sessionId,
  delivery,
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

  const preview = useMemo(() => buildCommentsMarkdown(comments, { isMultiRepo }), [comments, isMultiRepo]);

  const sendBlocked = busy || comments.length === 0 || !sendEnabled;
  // One tooltip covers every disabled reason so it never explains the wrong one.
  const sendTooltip = !sendEnabled
    ? sendDisabledReason
    : comments.length === 0
      ? "Add at least one diff comment to send."
      : busy
        ? "Sending your comments to the agent..."
        : "Send comments to agent";

  /** Sends the assembled prompt through the selected session transport; failures keep the draft editable. */
  const send = useCallback(async () => {
    if (busy || comments.length === 0 || !sendEnabled) return;
    setBusy(true);
    setError(null);
    const built = buildDiffCommentsPrompt(comments, introDraft, outroDraft, {
      isMultiRepo,
    });
    try {
      const res = await fetch(
        delivery === "structured"
          ? `/api/sessions/${encodeURIComponent(sessionId)}/acp/prompt/diff-comments`
          : `/api/sessions/${encodeURIComponent(sessionId)}/send`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(delivery === "structured" ? built : { message: built.assembledMarkdown }),
        },
      );
      if (!res.ok) {
        const text = (await res.text().catch(() => "")).slice(0, 500);
        if (mountedRef.current) {
          setError(`Failed to send (${res.status}). ${text}`.trim());
        }
        return;
      }
      // Counted only on a confirmed 2xx.
      reportTelemetrySeen("diff_comments");
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
  }, [busy, comments, introDraft, outroDraft, isMultiRepo, sendEnabled, sessionId, delivery, onSent]);

  // Document-level so textareas do not swallow the hotkeys; Esc is ignored mid-send.
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
    <Dialog
      id="send-comments"
      title="Send diff comments"
      describedBy={false}
      panelClassName="md:w-[42rem] md:max-h-[90vh]"
      bodyClassName="space-y-3"
      onDismiss={onClose}
      dismissOn="mousedown"
      headerEnd={
        <>
          <span className="shrink-0 text-[13px] md:text-[11px] text-text-dim">
            {comments.length} comment{comments.length === 1 ? "" : "s"}
          </span>
          <CloseButton onClick={onClose} />
        </>
      }
      footer={
        <>
          <label className="mr-auto max-md:basis-full max-md:min-h-11 flex items-center gap-2 text-[13px] md:text-[11px] text-text-dim cursor-pointer">
            <input
              type="checkbox"
              checked={clearAfterSend}
              onChange={(e) => onChangeClearAfterSend(e.target.checked)}
              className="cursor-pointer accent-brand-600"
            />
            Clear comments after sending
          </label>
          <CancelButton onClick={onClose} />
          {/* Tooltip + `aria-disabled` rather than `title` + `disabled`: neither a `title` nor a focus ring
              shows on a natively disabled button. `send()` re-checks every condition itself. */}
          <Tooltip text={sendTooltip} multiline>
            <button
              type="button"
              onClick={() => void send()}
              aria-disabled={sendBlocked}
              className={`${BUTTON_PRIMARY} ${sendBlocked ? "opacity-40 !cursor-not-allowed" : ""}`}
            >
              {busy ? "Sending..." : "Send"}
            </button>
          </Tooltip>
        </>
      }
    >
      <section>
        <label className={LABEL}>Intro (optional)</label>
        <textarea
          autoFocus={hasFinePointer()}
          value={introDraft}
          onChange={(e) => onChangeIntro(e.target.value)}
          placeholder="Anything you want to say before the comments..."
          rows={2}
          className={TEXTAREA}
        />
      </section>

      <section>
        <div className={LABEL}>Comments preview (auto-generated, read-only)</div>
        <div className="border border-surface-700/60 rounded-lg p-3 bg-surface-950 max-h-72 overflow-auto text-[13px]">
          {preview ? <CommentMarkdown text={preview} /> : <span className="text-text-dim italic">No comments.</span>}
        </div>
      </section>

      <section>
        <label className={LABEL}>Outro</label>
        <textarea
          value={outroDraft}
          onChange={(e) => onChangeOutro(e.target.value)}
          placeholder="Please address these comments."
          rows={2}
          className={TEXTAREA}
        />
      </section>

      {error && <div className="text-[13px] text-status-error bg-status-error/10 rounded-lg p-3">{error}</div>}
    </Dialog>
  );
}
