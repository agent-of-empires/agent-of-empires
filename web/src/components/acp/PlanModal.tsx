// Reading view for an ExitPlanMode approval: the full plan rendered or raw,
// with the card's decisions in the footer. Holds no approval state, so the
// card stays the single place a decision is submitted.

import { useState } from "react";
import { TextMessagePartProvider } from "@assistant-ui/react";

import type { ApprovalDecision } from "../../lib/acpTypes";
import { OFFLINE_TITLE } from "../../lib/connectionState";
import { BRAND_BUTTON, Dialog } from "../Dialog";
import { CopyMarkdownButton } from "./CopyMarkdownButton";
import { Markdown } from "./Markdown";

interface Props {
  plan: string;
  submitting: boolean;
  offline: boolean;
  onDecide: (decision: ApprovalDecision) => void;
  onClose: () => void;
}

const FOOTER_BUTTON = "px-3 py-1.5 text-sm rounded-md cursor-pointer transition-colors disabled:opacity-50";

export function PlanModal({ plan, submitting, offline, onDecide, onClose }: Props) {
  const [view, setView] = useState<"rendered" | "raw">("rendered");
  const blocked = offline || submitting;
  const decide = (label: string, decision: ApprovalDecision, className: string) => (
    <button
      type="button"
      className={`${FOOTER_BUTTON} ${className}`}
      disabled={blocked}
      title={offline ? OFFLINE_TITLE : undefined}
      onClick={() => onDecide(decision)}
    >
      {label}
    </button>
  );

  return (
    <Dialog
      id="plan-modal"
      title="Plan"
      panelClassName="w-[min(900px,94vw)] max-h-[90vh] flex flex-col"
      bodyClassName="min-h-0 flex-1 overflow-y-auto px-5 py-4"
      describedBy={false}
      onDismiss={onClose}
      footer={
        <>
          <button
            type="button"
            className={`${FOOTER_BUTTON} mr-auto text-text-secondary hover:text-text-primary hover:bg-surface-700/50`}
            onClick={onClose}
          >
            Close
          </button>
          {decide("Deny", "Deny", "border border-surface-700 text-text-secondary hover:bg-surface-700")}
          {decide("Always", "AllowAlways", "border border-surface-700 text-text-secondary hover:bg-surface-700")}
          {decide("Allow", "Allow", BRAND_BUTTON)}
        </>
      }
    >
      <div className="mb-3 flex items-center gap-1" role="tablist" aria-label="Plan view">
        {(["rendered", "raw"] as const).map((v) => (
          <button
            key={v}
            type="button"
            role="tab"
            aria-selected={view === v}
            className={[
              "rounded px-2 py-1 text-xs",
              view === v ? "bg-surface-700 text-text-primary" : "text-text-dim hover:text-text-secondary",
            ].join(" ")}
            onClick={() => setView(v)}
          >
            {v === "rendered" ? "Rendered" : "Raw"}
          </button>
        ))}
        <CopyMarkdownButton source={plan} label="Copy plan" className="ml-auto" />
      </div>
      {view === "rendered" ? (
        <TextMessagePartProvider text={plan}>
          <Markdown text={plan} />
        </TextMessagePartProvider>
      ) : (
        <pre className="whitespace-pre-wrap break-words font-mono text-xs text-text-secondary">{plan}</pre>
      )}
    </Dialog>
  );
}
