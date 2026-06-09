// Approval card. Renders a pending tool-call approval inline in the
// conversation, matching the visual language of ToolCards.tsx so it
// reads as part of the same flow rather than a separate widget.
//
// Destructive vs benign:
//   - Benign: single-tap "Allow" / "Always" / "Deny" trio.
//   - Destructive: "Allow" requires an 800ms hold (haptic on touch),
//     swipe is reserved for dismiss-only and never approves.
//
// Optimistic state shows a spinner until the server's broadcast removes
// the approval from AcpState.pendingApprovals.

import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Check, ChevronDown, Shield, X } from "lucide-react";
import type { Approval, ApprovalDecision } from "../../lib/acpTypes";
import { useServerDown, OFFLINE_TITLE } from "../../lib/connectionState";
import { hasArgsBody, parseJsonObject, previewFromArgs } from "../../lib/acpArgs";

interface Props {
  approval: Approval;
  onResolve: (decision: ApprovalDecision) => Promise<void>;
}

const LONG_PRESS_MS = 800;

export function ApprovalCard({ approval, onResolve }: Props) {
  const offline = useServerDown();
  const [phase, setPhase] = useState<"pending" | "submitting" | "rolled-back">("pending");
  const [progress, setProgress] = useState(0);
  const pressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const progressTimer = useRef<ReturnType<typeof setInterval> | null>(null);

  const raw = approval.tool_call.args_preview;
  // Benign approvals collapse to a one-line preview so the queue stays
  // scannable; destructive ones default expanded so the full command is
  // in view before a hold-to-allow. Either way the toggle stays under
  // user control and nothing re-expands it on plan approval. See #1767.
  const [expanded, setExpanded] = useState(approval.destructive);
  const preview = useMemo(() => previewFromArgs(raw), [raw]);
  const canExpand = useMemo(() => hasArgsBody(raw), [raw]);
  const showEmptyArgsState = raw.trim() === "";
  const Header = canExpand ? "button" : "div";

  useEffect(() => {
    return () => {
      if (pressTimer.current) clearTimeout(pressTimer.current);
      if (progressTimer.current) clearInterval(progressTimer.current);
    };
  }, []);

  const submit = useCallback(
    async (decision: ApprovalDecision) => {
      setPhase("submitting");
      try {
        await onResolve(decision);
      } catch {
        setPhase("rolled-back");
      }
    },
    [onResolve],
  );

  const startLongPress = () => {
    if (phase !== "pending") return;
    setProgress(0);
    progressTimer.current = setInterval(() => {
      setProgress((p) => Math.min(100, p + (100 / LONG_PRESS_MS) * 30));
    }, 30);
    pressTimer.current = setTimeout(() => {
      if (progressTimer.current) {
        clearInterval(progressTimer.current);
        progressTimer.current = null;
      }
      if (typeof navigator !== "undefined" && "vibrate" in navigator) {
        try {
          (navigator as Navigator & { vibrate?: (p: number) => void }).vibrate?.(20);
        } catch {
          // ignore
        }
      }
      void submit("Allow");
    }, LONG_PRESS_MS);
  };

  const cancelLongPress = () => {
    if (pressTimer.current) {
      clearTimeout(pressTimer.current);
      pressTimer.current = null;
    }
    if (progressTimer.current) {
      clearInterval(progressTimer.current);
      progressTimer.current = null;
    }
    setProgress(0);
  };

  return (
    <div
      className={[
        "my-2 overflow-hidden rounded-md border bg-surface-800/50 text-sm",
        approval.destructive ? "border-rose-900/60 bg-rose-950/20" : "border-brand-700/40 bg-brand-700/5",
      ].join(" ")}
      role="alertdialog"
      aria-label={`Approval needed: ${approval.tool_call.name}`}
    >
      <Header
        type={canExpand ? "button" : undefined}
        onClick={canExpand ? () => setExpanded((v) => !v) : undefined}
        aria-expanded={canExpand ? expanded : undefined}
        className={[
          "flex w-full items-center gap-2 px-3 py-2 text-left border-b border-surface-800/60",
          canExpand ? "cursor-pointer hover:bg-surface-800/40" : "",
        ].join(" ")}
      >
        {approval.destructive ? (
          <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-rose-400" />
        ) : (
          <Shield className="h-3.5 w-3.5 shrink-0 text-brand-500" />
        )}
        <span
          className={[
            "shrink-0 text-[11px] uppercase tracking-wider",
            approval.destructive ? "text-rose-400" : "text-brand-500",
          ].join(" ")}
        >
          {approval.destructive ? "Destructive action" : "Approval needed"}
        </span>
        <span className="shrink-0 font-mono text-xs text-text-secondary">{approval.tool_call.name}</span>
        {preview && <span className="min-w-0 flex-1 truncate font-mono text-xs text-text-dim">{preview}</span>}
        {canExpand && (
          <ChevronDown
            className={[
              "ml-auto h-3.5 w-3.5 shrink-0 text-text-dim transition-transform",
              expanded ? "rotate-180" : "",
            ].join(" ")}
          />
        )}
      </Header>

      {(expanded || showEmptyArgsState) && <ArgsView raw={raw} />}

      {phase === "rolled-back" && (
        <p className="px-3 pt-2 text-rose-400 text-xs">Could not reach the server. Tap to retry.</p>
      )}
      {offline && <p className="px-3 pt-2 text-status-error text-xs">{OFFLINE_TITLE}</p>}

      <div className="flex items-stretch gap-1.5 p-2">
        {approval.destructive ? (
          <button
            type="button"
            className={[
              "relative flex flex-1 items-center justify-center gap-1.5 overflow-hidden",
              "rounded-md text-white text-xs font-medium py-2 px-3",
              phase === "pending" ? "bg-rose-600 hover:bg-rose-500" : "bg-rose-700 opacity-70 cursor-wait",
            ].join(" ")}
            disabled={offline || (phase !== "pending" && phase !== "rolled-back")}
            onMouseDown={startLongPress}
            onMouseUp={cancelLongPress}
            onMouseLeave={cancelLongPress}
            onTouchStart={startLongPress}
            onTouchEnd={cancelLongPress}
            onTouchCancel={cancelLongPress}
          >
            <Check className="h-3.5 w-3.5 relative z-10" />
            <span className="relative z-10">{phase === "submitting" ? "Approving…" : "Hold to allow"}</span>
            <span
              className="absolute inset-0 bg-rose-400 origin-left"
              style={{ transform: `scaleX(${progress / 100})` }}
              aria-hidden="true"
            />
          </button>
        ) : (
          <>
            <button
              type="button"
              className={[
                "flex flex-1 items-center justify-center gap-1.5",
                "rounded-md text-white text-xs font-medium py-2 px-3",
                phase === "pending" ? "bg-brand-600 hover:bg-brand-500" : "bg-brand-700 opacity-70 cursor-wait",
              ].join(" ")}
              disabled={offline || phase !== "pending"}
              onClick={() => void submit("Allow")}
            >
              <Check className="h-3.5 w-3.5" />
              Allow
            </button>
            <button
              type="button"
              className={[
                "flex items-center justify-center gap-1.5",
                "rounded-md border border-surface-700 bg-surface-800",
                "text-xs font-medium text-text-secondary py-2 px-3",
                "hover:bg-surface-700",
                phase === "submitting" && "opacity-60 cursor-wait",
              ]
                .filter(Boolean)
                .join(" ")}
              disabled={offline || phase === "submitting"}
              onClick={() => void submit("AllowAlways")}
              title={offline ? OFFLINE_TITLE : "Allow this tool for the whole session"}
            >
              Always
            </button>
          </>
        )}

        <button
          type="button"
          className={[
            "flex items-center justify-center gap-1.5",
            "rounded-md border border-surface-700 bg-surface-800",
            "text-xs font-medium text-text-secondary py-2 px-3",
            "hover:border-rose-700/60 hover:bg-rose-950/30 hover:text-rose-300",
            phase === "submitting" && "opacity-60 cursor-wait",
          ]
            .filter(Boolean)
            .join(" ")}
          disabled={offline || phase === "submitting"}
          onClick={() => void submit("Deny")}
        >
          <X className="h-3.5 w-3.5" />
          Deny
        </button>
      </div>
    </div>
  );
}

// Maps a JSON args_preview to a definition list. Falls back to a raw
// <pre> when the payload doesn't parse as a plain object — preserves the
// original behaviour for arrays, primitives, and truncated previews
// (preview_args appends a "[truncated]" suffix that breaks JSON.parse).
function ArgsView({ raw }: { raw: string }) {
  const parsed = useMemo(() => parseJsonObject(raw), [raw]);

  // Gemini's confirm-required tools ship no raw_input, so the backend
  // sends an empty args_preview (rather than the literal "null"). Render
  // a dedicated empty-state instead of an empty <pre>. See #1713.
  if (raw.trim() === "") {
    return (
      <p className="border-b border-surface-800/60 bg-surface-950 px-3 py-2 font-mono text-[11px] italic text-text-dim">
        No raw args provided by agent.
      </p>
    );
  }

  if (!parsed) {
    return (
      <pre className="border-b border-surface-800/60 bg-surface-950 px-3 py-2 font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all max-h-32 overflow-y-auto">
        {raw}
      </pre>
    );
  }

  const entries = Object.entries(parsed).filter(([k]) => !k.startsWith("_aoe_"));
  if (entries.length === 0) return null;

  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5 border-b border-surface-800/60 bg-surface-950 px-3 py-2.5 max-h-48 overflow-y-auto text-xs">
      {entries.map(([k, v]) => (
        <Fragment key={k}>
          <dt className="font-mono text-[10px] uppercase tracking-wider text-text-dim self-start pt-0.5">{k}</dt>
          <dd className="font-mono text-text-secondary break-all whitespace-pre-wrap min-w-0">
            <ArgValue value={v} />
          </dd>
        </Fragment>
      ))}
    </dl>
  );
}

function ArgValue({ value }: { value: unknown }) {
  if (value === null) return <span className="text-text-dim italic">null</span>;
  if (value === undefined) return <span className="text-text-dim italic">—</span>;
  if (typeof value === "string") {
    return <>{value}</>;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return <span className="text-amber-300">{String(value)}</span>;
  }
  return (
    <pre className="font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all m-0">
      {JSON.stringify(value, null, 2)}
    </pre>
  );
}
