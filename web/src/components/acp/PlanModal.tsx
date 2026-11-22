// Reading view for an ExitPlanMode approval: the full plan rendered or raw,
// with the card's decisions in the footer. Holds no approval state, so the
// card stays the single place a decision is submitted.

import { type RefObject, useEffect, useMemo, useRef, useState } from "react";
import { TextMessagePartProvider } from "@assistant-ui/react";

import { tabStops, useFocusTrap } from "../../hooks/useFocusTrap";
import type { ApprovalDecision, ApprovalOption } from "../../lib/acpTypes";
import { OFFLINE_TITLE } from "../../lib/connectionState";
import type { FileRef } from "../../lib/fileRef";
import { BRAND_BUTTON, Dialog } from "../Dialog";
import { AcpFileRefContext, useAcpFileRef } from "./AcpFileRefContext";
import { CopyMarkdownButton } from "./CopyMarkdownButton";
import { Markdown } from "./Markdown";

interface Props {
  plan: string;
  submitting: boolean;
  offline: boolean;
  /** An answer list: its options are offered instead of the Allow/Always/Deny trio. */
  options?: ApprovalOption[];
  onDecide: (decision: ApprovalDecision, optionId?: string) => void;
  onClose: () => void;
}

// The root is found through a marker in this instance's own body, so retained plan
// modals in other sessions never act on each other's dialog.
const dialogOf = (marker: RefObject<HTMLSpanElement | null>) =>
  marker.current?.closest<HTMLElement>('[role="dialog"]') ?? null;

// Dialog contains no focus itself, so the plan modal moves focus in, closes on Escape
// and, through the trap, cycles Tab inside the panel and hands focus back to the opener.
function useModalFocus(onClose: () => void) {
  const marker = useRef<HTMLSpanElement>(null);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  });
  useFocusTrap(() => dialogOf(marker), { restoreFocus: true });
  useEffect(() => {
    const root = dialogOf(marker);
    if (root) tabStops(root)[0]?.focus();
    const onKey = (e: KeyboardEvent) => {
      // A covered session (inert) or a higher overlay such as the command palette owns the key.
      const el = dialogOf(marker);
      if (e.key !== "Escape" || !el || el.closest("[inert]")) return;
      const overlay = document.activeElement?.closest('[aria-modal="true"]');
      if (overlay && overlay !== el) return;
      e.preventDefault();
      e.stopPropagation();
      onCloseRef.current();
    };
    // Capture phase: the palette can unmount on this same Escape before a
    // bubbling listener runs, which would make the plan look like the owner.
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, []);
  return marker;
}

const SECONDARY_BUTTON = "border border-surface-700 text-text-secondary hover:bg-surface-700";
const FOOTER_BUTTON = "px-3 py-1.5 text-sm rounded-md cursor-pointer transition-colors disabled:opacity-50";

const VIEWS = ["rendered", "raw"] as const;
type View = (typeof VIEWS)[number];

export function PlanModal({ plan, submitting, offline, options, onDecide, onClose }: Props) {
  const [view, setView] = useState<View>("rendered");
  const marker = useModalFocus(onClose);
  // A plan link opens the file viewer behind the dialog, so the modal closes
  // first rather than leaving its Tab trap active over the viewer.
  const fileRefs = useAcpFileRef();
  const closingFileRefs = useMemo(() => {
    const open = fileRefs.onOpenFileRef;
    return {
      ...fileRefs,
      onOpenFileRef: open
        ? (ref: FileRef) => {
            onClose();
            open(ref);
          }
        : undefined,
    };
  }, [fileRefs, onClose]);
  const onTabKey = (e: React.KeyboardEvent, v: View) => {
    const at = VIEWS.indexOf(v);
    const next =
      e.key === "ArrowRight"
        ? VIEWS[(at + 1) % VIEWS.length]
        : e.key === "ArrowLeft"
          ? VIEWS[(at + VIEWS.length - 1) % VIEWS.length]
          : e.key === "Home"
            ? VIEWS[0]
            : e.key === "End"
              ? VIEWS[VIEWS.length - 1]
              : undefined;
    if (!next) return;
    e.preventDefault();
    setView(next);
    document.getElementById(`plan-tab-${next}`)?.focus();
  };
  const blocked = offline || submitting;
  const decide = (label: string, decision: ApprovalDecision, className: string, optionId?: string) => (
    <button
      key={optionId ?? decision}
      type="button"
      className={`${FOOTER_BUTTON} ${className}`}
      disabled={blocked}
      title={offline ? OFFLINE_TITLE : undefined}
      onClick={() => onDecide(decision, optionId)}
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
          {/* Same order as the card: answers or Allow first, then Always, then the way out. */}
          {options ? (
            <>
              {options.map((o) => decide(o.name, "Allow", BRAND_BUTTON, o.option_id))}
              {decide("Dismiss", "Cancelled", SECONDARY_BUTTON)}
            </>
          ) : (
            <>
              {decide("Allow", "Allow", BRAND_BUTTON)}
              {decide("Always", "AllowAlways", SECONDARY_BUTTON)}
              {decide("Deny", "Deny", SECONDARY_BUTTON)}
            </>
          )}
        </>
      }
    >
      <span ref={marker} hidden />
      <div className="mb-3 flex items-center gap-1" role="tablist" aria-label="Plan view">
        {VIEWS.map((v) => (
          <button
            key={v}
            id={`plan-tab-${v}`}
            type="button"
            role="tab"
            aria-selected={view === v}
            aria-controls="plan-panel"
            tabIndex={view === v ? 0 : -1}
            onKeyDown={(e) => onTabKey(e, v)}
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
      <div id="plan-panel" role="tabpanel" aria-labelledby={`plan-tab-${view}`}>
        {view === "rendered" ? (
          <AcpFileRefContext.Provider value={closingFileRefs}>
            <TextMessagePartProvider text={plan}>
              <Markdown text={plan} />
            </TextMessagePartProvider>
          </AcpFileRefContext.Provider>
        ) : (
          <pre className="whitespace-pre-wrap break-words font-mono text-xs text-text-secondary">{plan}</pre>
        )}
      </div>
    </Dialog>
  );
}
