import { Fragment, useEffect, useRef, useState } from "react";
import { AlertTriangle, Check, Clock, Info, Paperclip, RotateCcw, SendHorizontal, X } from "lucide-react";

import { useIsCoarsePointer } from "../../hooks/useIsCoarsePointer";
import type { QueuedPrompt, RejectedPrompt, SessionNotice } from "../../lib/acpTypes";
import { useAcpPrefs } from "../../lib/acpPrefs";
import { isClearAlias } from "../../lib/agentProfiles";
import { isQueuedPromptLong, queuedStripLayout } from "./queuedPromptsLayout";
import type { QueuedAction, QueuedReason } from "./queuedReason";
import { Tooltip } from "../Tooltip";

const AMBER_STRIP = "border-amber-900/40 bg-amber-950/20";
const AMBER_ROW = "flex items-start gap-2 rounded-lg border border-amber-700/30 bg-amber-950/15 px-2.5 py-1.5";
const AMBER_DISMISS =
  "inline-flex shrink-0 items-center justify-center rounded-md border border-amber-700/40 bg-amber-900/20 p-1 text-amber-200 hover:bg-amber-900/60";

/** Full-width strip above the composer, content aligned with the transcript column. */
function Strip({ tone, testId, children }: { tone: string; testId?: string; children: React.ReactNode }) {
  return (
    <div data-testid={testId} className={`border-t ${tone} px-4 py-2`}>
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

interface NoticeTone {
  strip: string;
  row: string;
  icon: string;
  dismiss: string;
}

// Solid theme surfaces: a translucent severity wash darkens a light theme's
// surface below AA for the text on it, so severity rides on border and icon.
// Browsers treat a setTimeout delay above 2^31 - 1 ms as 0, so a config
// written before the server-side cap could otherwise dismiss notices at once.
const MAX_TIMER_DELAY_MS = 2_147_483_647;

const NOTICE_ROW = "flex items-start gap-2 rounded-lg border bg-surface-850 px-2.5 py-1.5";
const NOTICE_DISMISS =
  "inline-flex shrink-0 items-center justify-center rounded-md border border-surface-700 p-1 text-text-primary hover:bg-surface-800";

const ERROR_TONE: NoticeTone = {
  strip: "border-red-700/40 bg-surface-900",
  row: `${NOTICE_ROW} border-red-700/40`,
  icon: "mt-0.5 h-4 w-4 shrink-0 text-status-error",
  dismiss: NOTICE_DISMISS,
};

const WARNING_TONE: NoticeTone = {
  strip: "border-amber-700/40 bg-surface-900",
  row: `${NOTICE_ROW} border-amber-700/40`,
  icon: "mt-0.5 h-4 w-4 shrink-0 text-status-warning",
  dismiss: NOTICE_DISMISS,
};

const INFO_TONE: NoticeTone = {
  strip: "border-sky-700/40 bg-surface-900",
  row: `${NOTICE_ROW} border-sky-700/40`,
  icon: "mt-0.5 h-4 w-4 shrink-0 text-accent-500",
  dismiss: NOTICE_DISMISS,
};

/** An unknown future ACP level reads as advisory rather than alarming. */
function noticeTone(severity: string): NoticeTone {
  if (severity === "error") return ERROR_TONE;
  if (severity === "warning") return WARNING_TONE;
  return INFO_TONE;
}

/** Live advisories the agent pushed outside the turn flow (an approaching rate
 *  limit, a model fallback). Dismissal is local to this tab. */
export function SessionNoticesStrip({
  notices: allNotices,
  onDismiss,
}: {
  notices: SessionNotice[];
  onDismiss: (id: string) => void;
}) {
  const { noticeDisplay, noticeTimeoutSecs } = useAcpPrefs();
  const notices = noticeDisplay === "hidden" ? allNotices.filter((n) => n.severity === "error") : allNotices;

  // One timer per advisory, started when it first appears and kept across renders so a
  // later notice cannot postpone an earlier one. Errors are never auto-dismissed.
  const timers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const onDismissRef = useRef(onDismiss);
  useEffect(() => {
    onDismissRef.current = onDismiss;
  }, [onDismiss]);
  useEffect(() => {
    const live = new Set(
      noticeDisplay === "timeout" ? notices.filter((n) => n.severity !== "error").map((n) => n.id) : [],
    );
    for (const [id, timer] of timers.current) {
      if (!live.has(id)) {
        clearTimeout(timer);
        timers.current.delete(id);
      }
    }
    for (const id of live) {
      if (!timers.current.has(id)) {
        timers.current.set(
          id,
          setTimeout(() => onDismissRef.current(id), Math.min(noticeTimeoutSecs * 1000, MAX_TIMER_DELAY_MS)),
        );
      }
    }
  }, [notices, noticeDisplay, noticeTimeoutSecs]);
  useEffect(() => {
    const pending = timers.current;
    return () => {
      for (const timer of pending.values()) clearTimeout(timer);
      pending.clear();
    };
  }, []);

  if (notices.length === 0) return null;
  const strongest = notices.some((n) => n.severity === "error")
    ? "error"
    : notices.some((n) => n.severity === "warning")
      ? "warning"
      : "info";
  return (
    <Strip tone={noticeTone(strongest).strip}>
      <ul className="space-y-1">
        {notices.map((notice) => {
          const tone = noticeTone(notice.severity);
          const Icon = notice.severity === "error" ? AlertTriangle : Info;
          return (
            <li key={notice.id} className={tone.row}>
              <Icon className={tone.icon} />
              <div className="min-w-0 flex-1">
                <p className="text-xs leading-5 text-text-primary">{notice.title}</p>
                {notice.description ? (
                  <p className="mt-0.5 text-[11px] leading-4 text-text-primary">{notice.description}</p>
                ) : null}
              </div>
              <button
                type="button"
                onClick={() => onDismiss(notice.id)}
                className={tone.dismiss}
                aria-label={`Dismiss notice: ${notice.title}`}
              >
                <X className="h-3 w-3" />
              </button>
            </li>
          );
        })}
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
  /** Why the queue is waiting, and what a row's action does about it. */
  reason: QueuedReason;
  /** Start the Session, after which the Daemon sends the queue. */
  onStartAndSend?: () => void;
  /** Stop the running turn, after which the Daemon drains the queue in order. */
  onStopTurn?: () => void;
  /** Commands that run as their own turn: clear and compact aliases. */
  soloAliases?: readonly string[];
  /** The Session's Agent, by display name, for the header's tooltip. */
  agent: string;
}

export function QueuedPromptsStrip({
  queued,
  onRemove,
  onEdit,
  onClear,
  onSendNow,
  canSendNow,
  reason,
  onStartAndSend,
  onStopTurn,
  soloAliases = NO_ALIASES,
  agent,
}: QueuedPromptsStripProps) {
  const isMobile = useIsCoarsePointer();
  const [expanded, setExpanded] = useState(false);
  if (queued.length === 0) return null;
  const layout = queuedStripLayout({ queuedCount: queued.length, isMobile, expanded });
  const visible = queued.slice(0, layout.visibleCount);
  return (
    <Strip tone="border-surface-800 bg-surface-900/60" testId="queued-strip">
      <div className="flex flex-wrap items-center justify-between gap-x-2 gap-y-0.5 pb-1.5 text-[11px] text-text-subtle">
        {/* The Daemon holds the queue, except rows it hasn't confirmed yet: those live only here. */}
        <Tooltip
          multiline
          text={
            queued.some((q) => q.pending)
              ? "A message isn't sent yet. This browser sends it when you're back online or reopen this chat."
              : `Sends when ${agent} is free, even if you close this tab.`
          }
        >
          <span
            tabIndex={0}
            data-testid="queued-strip-header"
            className="flex min-w-0 flex-wrap items-baseline gap-x-1 rounded focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500"
          >
            <span className="flex shrink-0 items-center gap-1 uppercase tracking-wider">
              <Clock className="h-3 w-3 shrink-0" />
              {`${reason.label} (${queued.length})`}
            </span>
            {/* Sentence case after the label; at a narrow width it wraps under it rather than truncating. */}
            {reason.reason && (
              <span data-testid="queued-strip-reason" className="min-w-0 wrap-anywhere">
                · {reason.reason}
              </span>
            )}
          </span>
        </Tooltip>
        {queued.length > 1 && (
          <button
            type="button"
            onClick={onClear}
            className="shrink-0 uppercase tracking-wider text-text-subtle hover:text-text-secondary transition-colors"
          >
            Clear all
          </button>
        )}
      </div>
      <ul className="flex flex-col gap-1.5">
        {visible.map((q, i) => {
          // A clear or compact command on either side means the drain posts these separately.
          const prev = i > 0 ? visible[i - 1] : undefined;
          const showDivider =
            soloAliases.length > 0 &&
            prev !== undefined &&
            (isClearAlias(prev.text, soloAliases) || isClearAlias(q.text, soloAliases));
          return (
            <Fragment key={q.id}>
              {showDivider && (
                <li
                  aria-hidden="true"
                  data-testid="queued-solo-boundary"
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
                onStartAndSend={onStartAndSend}
                onStopTurn={onStopTurn}
                canSendNow={canSendNow}
                action={isClearAlias(q.text, soloAliases) ? reason.soloAction : reason.action}
                disabledReason={
                  // Only a "sends …" reason reads as a sentence after "this message".
                  reason.disabledReason ??
                  (reason.reason?.startsWith("sends ") ? `Waiting: this message ${reason.reason}` : undefined)
                }
              />
            </Fragment>
          );
        })}
      </ul>
      {layout.toggleLabel && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-1.5 w-full rounded-md border border-dashed border-status-unread/40 px-2 py-1 text-[11px] font-medium uppercase tracking-wider text-text-subtle hover:bg-surface-800/60 hover:text-text-primary"
        >
          {layout.toggleLabel}
        </button>
      )}
    </Strip>
  );
}

const NO_ALIASES: readonly string[] = [];

const ACTION_TEXT: Partial<Record<QueuedAction, string>> = {
  stopAndSend: "Stop and send",
  stopAndRun: "Stop and run",
  startAndSend: "Start and send",
};

function QueuedPromptRow({
  prompt,
  onRemove,
  onEdit,
  onSendNow,
  onStartAndSend,
  onStopTurn,
  canSendNow,
  action,
  disabledReason,
}: {
  prompt: QueuedPrompt;
  onRemove: () => void;
  onEdit: (text: string) => void;
  onSendNow: () => void;
  onStartAndSend?: () => void;
  onStopTurn?: () => void;
  canSendNow: boolean;
  action: QueuedAction;
  disabledReason?: string;
}) {
  const [editing, setEditing] = useState(false);
  const [rowExpanded, setRowExpanded] = useState(false);
  const isLong = isQueuedPromptLong(prompt.text);

  return (
    <li className="group flex items-start gap-2 rounded-lg border border-dashed border-status-unread/60 bg-transparent px-2.5 py-1.5">
      <span className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-status-unread/15 text-[10px] font-semibold text-status-unread">
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
              className="mt-0.5 text-[11px] font-medium text-status-unread hover:text-text-primary"
              aria-label={rowExpanded ? "Collapse queued message" : "Show full queued message"}
            >
              {rowExpanded ? "Show less" : "…"}
            </button>
          )}
          {prompt.attachments && prompt.attachments.length > 0 && (
            <div className="mt-1 flex flex-wrap gap-1.5" data-testid="queued-attachments">
              {prompt.attachments.map((att, i) => (
                <span
                  key={`${att.name ?? att.kind}-${i}`}
                  className="flex items-center gap-1 rounded border border-status-unread/40 py-0.5 pl-0.5 pr-1.5 text-[10px] text-text-secondary"
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
      {!editing && action !== "dropOnly" && (
        <QueuedRowAction
          action={action}
          disabledReason={disabledReason}
          canSendNow={canSendNow}
          onSendNow={onSendNow}
          onStartAndSend={onStartAndSend}
          onStopTurn={onStopTurn}
        />
      )}
      <button
        type="button"
        onClick={onRemove}
        title="Drop this queued message"
        className="shrink-0 rounded p-1 text-text-subtle hover:bg-surface-800 hover:text-text-secondary"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </li>
  );
}

function QueuedRowAction({
  action,
  disabledReason,
  canSendNow,
  onSendNow,
  onStartAndSend,
  onStopTurn,
}: {
  action: QueuedAction;
  disabledReason?: string;
  canSendNow: boolean;
  onSendNow: () => void;
  onStartAndSend?: () => void;
  onStopTurn?: () => void;
}) {
  const text = ACTION_TEXT[action];
  const handler = action === "startAndSend" ? onStartAndSend : action === "stopAndRun" ? onStopTurn : onSendNow;
  // Starting a Session doesn't need a live Worker; every other action does.
  const available = action !== "disabled" && !!handler && (action === "startAndSend" || canSendNow);
  if (!available) {
    // `aria-disabled`, not `disabled`: a disabled button gets no hover or focus, so its reason
    // couldn't show.
    const why = disabledReason ?? "Send now needs this dashboard connected and the Agent running";
    return (
      <Tooltip text={why} multiline>
        <button
          type="button"
          aria-disabled="true"
          aria-label={why}
          data-testid="queued-send-now"
          className={`shrink-0 cursor-not-allowed rounded text-text-subtle opacity-60 ${text ? "px-1.5 py-0.5 text-[11px] font-medium" : "p-1"}`}
        >
          {text ?? <SendHorizontal className="h-3.5 w-3.5" />}
        </button>
      </Tooltip>
    );
  }
  if (text) {
    return (
      <button
        type="button"
        onClick={handler}
        data-testid="queued-send-now"
        className="shrink-0 rounded px-1.5 py-0.5 text-[11px] font-medium text-text-primary hover:bg-surface-800"
      >
        {text}
      </button>
    );
  }
  return (
    <button
      type="button"
      onClick={handler}
      title="Send this message now"
      aria-label="Send this queued message now"
      data-testid="queued-send-now"
      className="shrink-0 rounded p-1 text-status-unread hover:bg-surface-800 hover:text-text-primary"
    >
      <SendHorizontal className="h-3.5 w-3.5" />
    </button>
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
