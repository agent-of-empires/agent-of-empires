import { useState } from "react";
import { ChevronDown, ListChecks } from "lucide-react";

import type { Plan } from "../../lib/acpTypes";

const STEP_GLYPHS: Record<Plan["steps"][number]["status"], [string, string]> = {
  Done: ["text-status-running", "✓"],
  InProgress: ["text-brand-500", "●"],
  Cancelled: ["text-text-dim", "⊘"],
  Pending: ["text-text-dim", "○"],
};

const STEP_TITLE_CLASS: Partial<Record<Plan["steps"][number]["status"], string>> = {
  Done: "text-text-dim line-through",
  InProgress: "text-text-primary font-medium",
};

export function PlanStrip({ plan }: { plan: Plan | null }) {
  const [expanded, setExpanded] = useState(false);
  if (!plan || plan.steps.length === 0) return null;

  // Same active-step rule as the server's `plan_summary_from_plan`, so the strip and sidebar agree.
  const current =
    plan.steps.find((s) => s.status === "InProgress") ??
    plan.steps.find((s) => s.status !== "Done" && s.status !== "Cancelled");
  const completed = plan.steps.filter((s) => s.status === "Done").length;
  const totalSteps = plan.steps.length;
  const pct = Math.round((completed / totalSteps) * 100);
  const allDone = completed === totalSteps;

  return (
    <div className="border-b border-surface-800 bg-surface-900/95 backdrop-blur">
      <button
        type="button"
        className="flex w-full items-center gap-3 px-4 py-2 text-left text-sm hover:bg-surface-800/40"
        onClick={() => setExpanded((v) => !v)}
      >
        <ListChecks className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        <span className="truncate text-text-primary">{current?.title ?? (allDone ? "all steps complete" : "…")}</span>
        <span className="ml-auto flex items-center gap-2">
          <span className="text-[11px] tabular-nums text-text-dim">
            {completed}/{totalSteps}
          </span>
          <span className="hidden sm:block h-1 w-16 overflow-hidden rounded-full bg-surface-800">
            <span className="block h-full bg-brand-500 transition-[width] duration-300" style={{ width: `${pct}%` }} />
          </span>
          <ChevronDown
            className={["h-3.5 w-3.5 text-text-dim transition-transform", expanded ? "rotate-180" : ""].join(" ")}
          />
        </span>
      </button>

      {expanded && (
        <div className="max-h-64 overflow-y-auto border-t border-surface-800 px-4 py-2 text-sm">
          <ul className="space-y-1">
            {plan.steps.map((step) => {
              const [glyphClass, glyph] = STEP_GLYPHS[step.status] ?? STEP_GLYPHS.Pending;
              return (
                <li key={step.id} className="flex items-start gap-2 text-text-secondary">
                  <span className={glyphClass}>{glyph}</span>
                  <span className={STEP_TITLE_CLASS[step.status] ?? "text-text-secondary"}>{step.title}</span>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
}
