// Composer footer controls: toolbar, mode picker, usage hint, attachments, send/stop, popover list.

import { useEffect, useRef, useState } from "react";
import { ComposerPrimitive, useAui } from "@assistant-ui/react";
import { ChevronUp, Paperclip, Square, X } from "lucide-react";

import type { AcpState, PromptAttachmentInput } from "../../lib/acpTypes";
import { useAgentProfile } from "../../lib/agentProfileContext";
import { resolveModeChannel } from "../../lib/modeChannel";
import { badgeLabel, badgeTone, resolveSkillSource, type SkillIndex } from "../../lib/skillProvenance";
import { TOUR_ANCHORS, tourAnchor } from "../../lib/tourSteps";
import { ProvenanceBadge } from "../ProvenanceBadge";
import { Tooltip } from "../Tooltip";

/** Flat item list shared by the `@` and `/` popovers; `/` passes a skill index for provenance badges. */
export function PopoverItems({ trigger, skillIndex }: { trigger: string; skillIndex?: SkillIndex }) {
  return (
    <ComposerPrimitive.Unstable_TriggerPopoverItems className="max-h-64 overflow-y-auto">
      {(items) =>
        items.length === 0 ? (
          <div className="px-3 py-2 text-xs italic text-text-dim">No matches</div>
        ) : (
          items.map((item, i) => {
            const source = skillIndex ? resolveSkillSource(skillIndex, item.id) : null;
            return (
              <ComposerPrimitive.Unstable_TriggerPopoverItem
                key={item.id}
                item={item}
                index={i}
                className={[
                  "flex w-full items-start gap-2 px-3 py-2 text-left text-xs",
                  "hover:bg-surface-800/60",
                  "data-[highlighted=true]:bg-surface-800",
                ].join(" ")}
              >
                <span className="font-mono text-text-dim">{trigger}</span>
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-1.5">
                    <span className="block truncate font-medium text-text-primary">{item.label}</span>
                    {source && <ProvenanceBadge label={badgeLabel(source)} tone={badgeTone(source)} />}
                  </span>
                  {item.description && (
                    <span className="block truncate text-[11px] text-text-dim">{item.description}</span>
                  )}
                </span>
              </ComposerPrimitive.Unstable_TriggerPopoverItem>
            );
          })
        )
      }
    </ComposerPrimitive.Unstable_TriggerPopoverItems>
  );
}

export function AttachmentChips({
  attachments,
  onRemove,
}: {
  attachments: PromptAttachmentInput[];
  onRemove: (index: number) => void;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-2 px-3 pt-1">
      {attachments.map((att, i) => (
        <div
          key={`${att.name ?? att.kind}-${i}`}
          className="group/att relative flex items-center gap-2 rounded-md border border-surface-700 bg-surface-800 py-1 pl-1 pr-2 text-[11px] text-text-secondary"
        >
          {att.kind === "image" ? (
            <img
              src={`data:${att.mimeType};base64,${att.dataB64}`}
              alt={att.name ?? "attachment"}
              className="h-8 w-8 rounded object-cover"
            />
          ) : (
            <span className="flex h-8 w-8 items-center justify-center rounded bg-surface-700">
              <Paperclip className="h-3.5 w-3.5" />
            </span>
          )}
          <span className="max-w-[120px] truncate">{att.name ?? att.kind}</span>
          <button
            type="button"
            aria-label={`Remove ${att.name ?? "attachment"}`}
            title="Remove attachment"
            onClick={() => onRemove(i)}
            className="rounded p-0.5 text-text-dim hover:bg-surface-700 hover:text-text-secondary"
          >
            <X className="h-3 w-3" />
          </button>
        </div>
      ))}
    </div>
  );
}

export function ToolbarButton({
  icon,
  label,
  hint,
  disabled,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className={[
        "inline-flex items-center gap-1 rounded-md px-2 py-1 text-[11px] text-text-dim",
        "hover:bg-surface-800 hover:text-text-secondary",
        "disabled:cursor-not-allowed disabled:opacity-60 disabled:hover:bg-transparent disabled:hover:text-text-dim",
        "transition-colors",
      ].join(" ")}
    >
      {icon}
      {hint && <span className="font-mono">{hint}</span>}
    </button>
  );
}

/** Legacy `session/set_mode`; success arrives as a CurrentModeChanged broadcast. */
async function postLegacyMode(sessionId: string, id: string): Promise<void> {
  try {
    await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/mode`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ mode_id: id }),
    });
  } catch {
    // On failure the UI simply stays on the current mode.
  }
}

/** Mode chips tinted by id so destructive modes stand out. */
const MODE_TONES: [RegExp, string][] = [
  [/bypass|yolo/i, "border-rose-700/50 bg-rose-950/30 text-rose-300 hover:border-rose-700"],
  [/accept/i, "border-amber-700/50 bg-amber-950/30 text-amber-300 hover:border-amber-700"],
  [/plan/i, "border-cyan-800/50 bg-cyan-950/30 text-cyan-300 hover:border-cyan-700"],
];
const DEFAULT_MODE_TONE = "border-surface-700 bg-surface-800 text-text-secondary hover:border-surface-600";

export function ModePicker({
  sessionId,
  availableModes,
  currentModeId,
  legacyMode,
  configOptions,
  pendingConfigOption,
  setConfigOption,
}: {
  sessionId: string;
  availableModes: AcpState["availableModes"];
  currentModeId: string | null;
  legacyMode: AcpState["mode"];
  configOptions: AcpState["configOptions"];
  pendingConfigOption: AcpState["pendingConfigOption"];
  setConfigOption: (configId: string, value: string) => void | Promise<void>;
}) {
  const profile = useAgentProfile();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);
  // Each channel (config option, SessionModeState, claude fallback) pairs with its own write path.
  const channel = resolveModeChannel({
    configOptions,
    availableModes,
    currentModeId,
    legacyMode,
    pendingConfigOption,
    allowLegacyFallback: profile.capabilities.legacyModeFallback,
  });

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  if (!channel) return null;
  const current = channel.modes.find((m) => m.id === channel.activeId) ?? channel.modes[0]!;
  const tone = MODE_TONES.find(([re]) => re.test(channel.activeId))?.[1] ?? DEFAULT_MODE_TONE;

  const select = (id: string) => {
    setOpen(false);
    if (id === channel.activeId || id === channel.pendingId) return;
    if (channel.kind === "config") void setConfigOption(channel.configId, id);
    else void postLegacyMode(sessionId, id);
  };

  return (
    <div ref={ref} {...tourAnchor(TOUR_ANCHORS.modePicker)} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        title={current.description || `Mode: ${current.name}`}
        className={[
          "inline-flex items-center gap-1 rounded-md border px-2 py-1 text-[11px] font-medium",
          "transition-colors",
          tone,
        ].join(" ")}
      >
        <span>{current.name}</span>
        <ChevronUp className="h-3 w-3 opacity-70" />
      </button>
      {open && (
        <div
          className="absolute bottom-full left-0 z-30 mb-1 w-56 overflow-hidden rounded-md border border-surface-700 bg-surface-850 shadow-xl"
          role="menu"
        >
          <div className="border-b border-surface-800 px-3 py-1.5 text-[10px] uppercase tracking-wider text-text-dim">
            {channel.label}
          </div>
          {channel.modes.map((opt) => {
            const isPending = opt.id === channel.pendingId;
            const isActive = opt.id === channel.activeId;
            return (
              <button
                key={opt.id}
                type="button"
                role="menuitem"
                disabled={isPending}
                onClick={() => select(opt.id)}
                className={[
                  "flex w-full items-start gap-2 px-3 py-2 text-left text-xs hover:bg-surface-800",
                  isActive ? "bg-surface-800/60" : "",
                  isPending ? "cursor-not-allowed opacity-50" : "",
                ].join(" ")}
              >
                <span
                  className={[
                    "mt-0.5 inline-block h-3 w-3 shrink-0 rounded-full border",
                    isActive ? "border-brand-500 bg-brand-500" : "border-surface-700",
                  ].join(" ")}
                />
                <span className="min-w-0 flex-1">
                  <span className="block font-medium text-text-primary">{opt.name}</span>
                  {opt.description && <span className="block text-[11px] text-text-dim">{opt.description}</span>}
                </span>
                {isPending && <span className="text-[10px] uppercase text-text-dim">…</span>}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

function formatTokens(n: number): string {
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return `${(n / 1_000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 2 : 1)}M`;
}

function formatCost(amount: number, currency: string): string {
  try {
    return new Intl.NumberFormat(undefined, {
      style: "currency",
      currency,
      maximumFractionDigits: amount < 1 ? 4 : 2,
    }).format(amount);
  } catch {
    return `${amount.toFixed(amount < 1 ? 4 : 2)} ${currency}`;
  }
}

export function UsageHint({ usage }: { usage: AcpState["sessionUsage"] }) {
  if (!usage || usage.size <= 0) return null;
  const pct = Math.min(100, Math.round((usage.used / usage.size) * 100));
  const tone = pct >= 90 ? "text-rose-400" : pct >= 75 ? "text-amber-400" : "text-text-dim";
  const cost = usage.cost ? formatCost(usage.cost.amount, usage.cost.currency) : null;
  const explanation =
    `Context window: ${usage.used.toLocaleString()} of ${usage.size.toLocaleString()} tokens used (${pct}%). ` +
    `Color warns as the window fills.` +
    (cost ? ` ${cost} is cumulative session spend since the last /clear or /compact.` : "");
  // Last in the wrapping cluster: on a narrow footer it takes its own row
  // instead of pushing Stop and Send off screen.
  return (
    <span className="ml-auto pl-2">
      <Tooltip text={explanation} multiline>
        <span
          data-testid="composer-usage"
          className={`inline-flex items-center gap-1 text-[11px] tabular-nums ${tone}`}
          aria-label={explanation}
        >
          <span className="hidden sm:inline">
            {formatTokens(usage.used)}/{formatTokens(usage.size)}
          </span>
          <span className="opacity-70">
            <span className="hidden sm:inline">(</span>
            {pct}%<span className="hidden sm:inline">)</span>
          </span>
          {cost ? <span className="opacity-70">· {cost}</span> : null}
        </span>
      </Tooltip>
    </span>
  );
}

function sendButtonClass(disabled: boolean, extra = "") {
  return [
    `group/send ${extra}inline-flex items-center justify-center gap-1`,
    "rounded-lg bg-brand-600 px-2.5 py-1.5 text-white shadow-sm",
    "transition-all duration-100",
    disabled ? "cursor-not-allowed opacity-40" : "hover:bg-brand-500 active:scale-[0.98]",
  ].join(" ");
}

/** Stays clickable while disconnected: `sendPrompt` queues until the session resumes. */
export function SendButton({
  connected = true,
  disabled = false,
  onSend,
}: {
  connected?: boolean;
  disabled?: boolean;
  onSend: () => void;
}) {
  const title = disabled
    ? "Type a message to send"
    : connected
      ? "Send, Enter"
      : "Session not active, will send on resume";
  return (
    <button
      type="button"
      aria-label={connected ? "Send message" : "Queue message until session resumes"}
      title={title}
      onClick={onSend}
      disabled={disabled}
      className={sendButtonClass(disabled)}
    >
      <PaperPlaneIcon />
    </button>
  );
}

export function StopButton() {
  const aui = useAui();
  return (
    <button
      type="button"
      aria-label="Stop"
      title="Stop the agent"
      onClick={() => aui.thread.cancelRun()}
      className={[
        "inline-flex items-center justify-center gap-1.5",
        "rounded-lg border border-surface-600 bg-surface-800",
        "px-2.5 py-1.5 text-[12px] font-medium text-text-secondary",
        "hover:border-rose-700/60 hover:bg-rose-950/30 hover:text-rose-300",
        "active:scale-[0.98] transition-all duration-100",
      ].join(" ")}
    >
      <Square className="h-3.5 w-3.5 fill-current" strokeWidth={0} />
      <span>Stop</span>
    </button>
  );
}

/** Send beside Stop mid-turn. A steerable, connected agent takes the message into
 *  the running turn; otherwise it queues. */
export function QueueSendButton({
  connected,
  steering,
  disabled = false,
  onSend,
}: {
  connected: boolean;
  steering: boolean;
  disabled?: boolean;
  onSend: () => void;
}) {
  const title = disabled
    ? "Type a message to queue"
    : !connected
      ? "Queue follow-up, will send on resume, Enter"
      : steering
        ? "Send into the current turn, Enter"
        : "Queue follow-up (sent when current turn ends), Enter";
  return (
    <button
      type="button"
      aria-label={connected && steering ? "Send message into the current turn" : "Queue follow-up message"}
      {...tourAnchor(TOUR_ANCHORS.queueSend)}
      title={title}
      onClick={onSend}
      disabled={disabled}
      className={sendButtonClass(disabled, "relative ")}
    >
      <PaperPlaneIcon />
    </button>
  );
}

function PaperPlaneIcon() {
  return (
    <svg
      viewBox="0 0 24 24"
      width="14"
      height="14"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M22 2 11 13" />
      <path d="M22 2 15 22l-4-9-9-4 20-7Z" />
    </svg>
  );
}
