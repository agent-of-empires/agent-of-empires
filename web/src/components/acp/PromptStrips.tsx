import { Fragment, useState } from "react";
import { AlertTriangle, Check, Clock, Info, Paperclip, RotateCcw, SendHorizontal, X } from "lucide-react";

import { useIsCoarsePointer } from "../../hooks/useIsCoarsePointer";
import type { QueuedPrompt, RejectedPrompt } from "../../lib/acpTypes";
import { useClearAliases } from "../../lib/agentProfileContext";
import { isClearAlias } from "../../lib/agentProfiles";
import { isQueuedPromptLong, queuedStripLayout } from "./queuedPromptsLayout";

const AMBER_STRIP = "border-amber-900/40 bg-amber-950/20";
const AMBER_ROW = "flex items-start gap-2 rounded-lg border border-amber-700/30 bg-amber-950/15 px-2.5 py-1.5";
const AMBER_DISMISS =
  "inline-flex shrink-0 items-center justify-center rounded-md border border-amber-700/40 bg-amber-900/20 p-1 text-amber-200 hover:bg-amber-900/60";

/** Full-width strip above the composer, content aligned with the transcript column. */
function Strip({ tone, children }: { tone: string; children: React.ReactNode }) {
  return (
    <div className={`border-t ${tone} px-4 py-2`}>
      <div className="mx-auto max-w-3xl xl:max-w-4xl 2xl:max-w-5xl">{children}</div>
    </div>
  );
}

/** Prompts the daemon refused while another turn was in flight. */
export function RejectedPromptsStrip({
  rejected,
  onRetry,
  onDismiss,
  disabled,
}: {
  rejected: RejectedPrompt[];
  onRetry: (text: string) => void;
  onDismiss: (id: string) => void;
  /** Retry is gated while the worker restarts: sending would clear the restart
   *  state before the respawn has actually reconnected. */
  disabled: boolean;
}) {
  if (rejected.length === 0) return null;
  return (
    <Strip tone={AMBER_STRIP}>
      <div className="pb-1.5 text-[11px] uppercase tracking-wider text-amber-300">
        <span className="inline-flex items-center gap-1">
          <AlertTriangle className="h-3 w-3" />
          Rejected ({rejected.length})
        </span>
      </div>
      <ul className="flex flex-col gap-1.5">
        {rejected.map((r) => (
          <li key={r.id} className={`group ${AMBER_ROW}`}>
            <span className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-amber-500/20 text-[10px] font-semibold text-amber-300">
              !
            </span>
            <div className="min-w-0 flex-1">
              {/* Bounded so a huge paste cannot push the composer off-screen. */}
              <p className="max-h-48 overflow-y-auto whitespace-pre-wrap break-words text-xs text-amber-100">
                {r.text}
              </p>
              <p className="mt-0.5 text-[10px] text-amber-400/80">Agent was busy; prompt was not sent.</p>
            </div>
            <button
              type="button"
              onClick={() => onRetry(r.text)}
              disabled={disabled}
              className="inline-flex shrink-0 items-center gap-1 rounded-md border border-amber-700/60 bg-amber-900/30 px-2 py-1 text-[10px] font-mono uppercase tracking-wide text-amber-100 hover:bg-amber-900/60 disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-amber-900/30"
              aria-label="Retry rejected prompt"
            >
              <RotateCcw className="h-3 w-3" />
              Retry
            </button>
            <button
              type="button"
              onClick={() => onDismiss(r.id)}
              className={AMBER_DISMISS}
              aria-label="Dismiss rejected prompt"
            >
              <X className="h-3 w-3" />
            </button>
          </li>
        ))}
      </ul>
    </Strip>
  );
}

/** Shown when the adapter rejected `session/set_mode`, typically bypassPermissions
 *  on a claude-agent-acp build without ALLOW_BYPASS. */
export function ModeSwitchFailedNotice({
  failure,
  onDismiss,
}: {
  failure: { modeId: string; reason: string; at: string } | null;
  onDismiss: () => void;
}) {
  if (!failure) return null;
  const friendly =
    failure.modeId === "bypassPermissions"
      ? "YOLO mode (bypassPermissions) is not available on this adapter; the session is running in default permission mode. claude-agent-acp gates bypass on the ALLOW_BYPASS env var. Pick a different mode from the composer or restart the daemon with ALLOW_BYPASS=1."
      : `Could not switch to mode "${failure.modeId}"; the session is staying on its previous mode. Pick a different mode from the composer.`;
  return (
    <Strip tone={AMBER_STRIP}>
      <div className={AMBER_ROW}>
        <Info className="mt-0.5 h-4 w-4 shrink-0 text-amber-300" />
        <div className="min-w-0 flex-1">
          <p className="text-xs leading-5 text-amber-100">{friendly}</p>
          <p className="mt-0.5 font-mono text-[10px] text-amber-400/70">{failure.reason}</p>
        </div>
        <button type="button" onClick={onDismiss} className={AMBER_DISMISS} aria-label="Dismiss mode-switch notice">
          <X className="h-3 w-3" />
        </button>
      </div>
    </Strip>
  );
}

interface QueuedPromptsStripProps {
  queued: QueuedPrompt[];
  onRemove: (id: string) => void;
  onEdit: (id: string, text: string) => void;
  onClear: () => void;
  /** Send now when the agent is free, or interrupt a running non-steerable turn. */
  onSendNow: (prompt: QueuedPrompt) => void;
  canSendNow: boolean;
  sendNowInterrupts: boolean;
  /** The drain cannot fire yet (disconnected, stopped, restarting, cold-starting). */
  pendingResume: boolean;
}

export function QueuedPromptsStrip({
  queued,
  onRemove,
  onEdit,
  onClear,
  onSendNow,
  canSendNow,
  sendNowInterrupts,
  pendingResume,
}: QueuedPromptsStripProps) {
  const isMobile = useIsCoarsePointer();
  const [expanded, setExpanded] = useState(false);
  const aliases = useClearAliases();
  if (queued.length === 0) return null;
  const layout = queuedStripLayout({ queuedCount: queued.length, isMobile, expanded });
  const visible = queued.slice(0, layout.visibleCount);
  return (
    <Strip tone="border-surface-800 bg-surface-900/60">
      <div className="flex items-center justify-between pb-1.5 text-[11px] uppercase tracking-wider text-text-dim">
        <span
          className="inline-flex items-center gap-1"
          title="Queued in this browser. Sends when the agent is free, even from another chat, as long as a dashboard tab stays open. Three closed chats deliver in the background at a time; the rest wait for a slot or for you to open them. Your other devices do not see it."
        >
          <Clock className="h-3 w-3" />
          {pendingResume ? `Pending until session resumes (${queued.length})` : `Queued (${queued.length})`}
        </span>
        {queued.length > 1 && (
          <button type="button" onClick={onClear} className="text-text-dim hover:text-text-secondary transition-colors">
            Clear all
          </button>
        )}
      </div>
      <ul className="flex flex-col gap-1.5">
        {visible.map((q, i) => {
          // A clear alias on either side means the drain posts these separately.
          const prev = i > 0 ? visible[i - 1] : undefined;
          const showDivider =
            aliases.length > 0 &&
            prev !== undefined &&
            (isClearAlias(prev.text, aliases) || isClearAlias(q.text, aliases));
          return (
            <Fragment key={q.id}>
              {showDivider && (
                <li
                  aria-hidden="true"
                  data-testid="queued-clear-boundary"
                  className="flex items-center gap-2 px-1 text-[10px] uppercase tracking-wider text-amber-300/60"
                >
                  <span className="h-px flex-1 bg-amber-500/20" />
                  fires separately
                  <span className="h-px flex-1 bg-amber-500/20" />
                </li>
              )}
              <QueuedPromptRow
                prompt={q}
                onRemove={() => onRemove(q.id)}
                onEdit={(text) => onEdit(q.id, text)}
                onSendNow={() => onSendNow(q)}
                canSendNow={canSendNow}
                sendNowInterrupts={sendNowInterrupts}
              />
            </Fragment>
          );
        })}
      </ul>
      {layout.toggleLabel && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-1.5 w-full rounded-md border border-sky-700/20 bg-sky-950/10 px-2 py-1 text-[11px] font-medium uppercase tracking-wider text-sky-300 hover:bg-sky-950/30"
        >
          {layout.toggleLabel}
        </button>
      )}
    </Strip>
  );
}

function QueuedPromptRow({
  prompt,
  onRemove,
  onEdit,
  onSendNow,
  canSendNow,
  sendNowInterrupts,
}: {
  prompt: QueuedPrompt;
  onRemove: () => void;
  onEdit: (text: string) => void;
  onSendNow: () => void;
  canSendNow: boolean;
  sendNowInterrupts: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const [rowExpanded, setRowExpanded] = useState(false);
  const isLong = isQueuedPromptLong(prompt.text);

  return (
    <li className="group flex items-start gap-2 rounded-lg border border-sky-700/30 bg-sky-950/15 px-2.5 py-1.5">
      <span className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-sky-500/20 text-[10px] font-semibold text-sky-300">
        ⏱
      </span>
      {editing ? (
        <QueuedPromptEditor
          key={prompt.id}
          initial={prompt.text}
          onCancel={() => setEditing(false)}
          onSave={(text) => {
            const trimmed = text.trim();
            if (trimmed && trimmed !== prompt.text) onEdit(trimmed);
            setEditing(false);
          }}
        />
      ) : (
        <div className="min-w-0 flex-1">
          {/* Expanded text scrolls in a capped box; the toggle stays outside it so it remains reachable. */}
          <div className={isLong && rowExpanded ? "max-h-48 overflow-y-auto" : ""}>
            <button
              type="button"
              onClick={() => setEditing(true)}
              title="Click to edit"
              className={[
                "w-full text-left text-xs leading-5 text-text-secondary whitespace-pre-wrap break-words hover:text-text-primary",
                // `block` overrides line-clamp's -webkit-box display, so the two must stay exclusive.
                isLong && !rowExpanded ? "line-clamp-3" : "block",
              ].join(" ")}
            >
              {prompt.text}
            </button>
          </div>
          {isLong && (
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                setRowExpanded((v) => !v);
              }}
              className="mt-0.5 text-[11px] font-medium text-sky-300 hover:text-sky-200"
              aria-label={rowExpanded ? "Collapse queued prompt" : "Show full queued prompt"}
            >
              {rowExpanded ? "Show less" : "…"}
            </button>
          )}
          {prompt.attachments && prompt.attachments.length > 0 && (
            <div className="mt-1 flex flex-wrap gap-1.5" data-testid="queued-attachments">
              {prompt.attachments.map((att, i) => (
                <span
                  key={`${att.name ?? att.kind}-${i}`}
                  className="flex items-center gap-1 rounded border border-sky-700/40 bg-sky-950/30 py-0.5 pl-0.5 pr-1.5 text-[10px] text-sky-200"
                  title={att.name ?? att.kind}
                >
                  {att.kind === "image" && att.dataB64 ? (
                    <img
                      src={`data:${att.mimeType};base64,${att.dataB64}`}
                      alt={att.name ?? "attachment"}
                      className="h-5 w-5 rounded object-cover"
                    />
                  ) : (
                    // Server-hydrated rows carry metadata only, no bytes.
                    <Paperclip className="h-3 w-3" />
                  )}
                  <span className="max-w-[100px] truncate">{att.name ?? att.kind}</span>
                </span>
              ))}
            </div>
          )}
        </div>
      )}
      {!editing && (
        <button
          type="button"
          onClick={onSendNow}
          disabled={!canSendNow}
          title={
            !canSendNow
              ? "Waiting to resume; this sends automatically once the session is back"
              : sendNowInterrupts
                ? "Stop the current turn and send this message now"
                : "Send this message now"
          }
          aria-label={
            sendNowInterrupts ? "Stop the current turn and send this queued message" : "Send this queued message now"
          }
          data-testid="queued-send-now"
          className="shrink-0 rounded p-1 text-sky-300 hover:bg-surface-800 hover:text-sky-200 disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-sky-300"
        >
          <SendHorizontal className="h-3.5 w-3.5" />
        </button>
      )}
      <button
        type="button"
        onClick={onRemove}
        title="Drop this queued message"
        className="shrink-0 rounded p-1 text-text-dim hover:bg-surface-800 hover:text-text-secondary"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </li>
  );
}

function QueuedPromptEditor({
  initial,
  onCancel,
  onSave,
}: {
  initial: string;
  onCancel: () => void;
  onSave: (text: string) => void;
}) {
  const [draft, setDraft] = useState(initial);
  return (
    <>
      <textarea
        autoFocus
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => onSave(draft)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            onSave(draft);
          } else if (e.key === "Escape") {
            e.preventDefault();
            onCancel();
          }
        }}
        rows={Math.min(6, Math.max(1, draft.split("\n").length))}
        className="min-w-0 flex-1 resize-none bg-transparent text-xs leading-5 text-text-primary outline-none placeholder:text-text-dim"
      />
      <button
        type="button"
        onMouseDown={(e) => e.preventDefault()}
        onClick={() => onSave(draft)}
        title="Save (Enter)"
        className="shrink-0 rounded p-1 text-text-dim hover:bg-surface-800 hover:text-emerald-300"
      >
        <Check className="h-3.5 w-3.5" />
      </button>
    </>
  );
}
