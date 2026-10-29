// Reading view for an ExitPlanMode approval: the full plan rendered or raw,
// with the card's decisions in the footer. Holds no approval state, so the
// card stays the single place a decision is submitted.

import { useEffect, useRef, useState } from "react";
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

const FOCUSABLE = "button:not([disabled])";

// Dialog contains no focus itself, so the plan modal moves focus in, cycles Tab
// inside the panel, closes on Escape and hands focus back to the opener.
function useModalFocus(onClose: () => void) {
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  });
  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    const root = () => document.querySelector<HTMLElement>('[data-testid="plan-modal"]');
    root()?.querySelector<HTMLElement>(FOCUSABLE)?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCloseRef.current();
        return;
      }
      if (e.key !== "Tab") return;
      const items = Array.from(root()?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? []);
      const first = items.at(0);
      const last = items.at(-1);
      if (!first || !last) return;
      const active = document.activeElement;
      if (!root()?.contains(active) || (e.shiftKey && active === first)) {
        e.preventDefault();
        (e.shiftKey && active === first ? last : first).focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      opener?.focus();
    };
  }, []);
}

const FOOTER_BUTTON = "px-3 py-1.5 text-sm rounded-md cursor-pointer transition-colors disabled:opacity-50";

export function PlanModal({ plan, submitting, offline, onDecide, onClose }: Props) {
  const [view, setView] = useState<"rendered" | "raw">("rendered");
  useModalFocus(onClose);
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
