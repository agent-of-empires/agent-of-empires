import { ArrowRight, X } from "lucide-react";
import { Link } from "react-router-dom";
import type { SetupStep } from "../../lib/api";
import { SETUP_STEP_INFO } from "../../lib/setup";

interface Props {
  /** Every step, for the count. */
  steps: SetupStep[];
  /** The rows to show: steps not configured yet. */
  open: SetupStep[];
  onDismiss: () => void;
}

export function SetupBanner({ steps, open, onDismiss }: Props) {
  const done = steps.length - open.length;
  const percent = Math.round((done / steps.length) * 100);
  return (
    <section
      aria-label="Finish setting up AoE"
      className="w-full max-w-2xl mb-6 rounded-lg border border-surface-700 bg-surface-900"
    >
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 pt-3">
        <h2 className="text-sm font-semibold text-text-primary">Finish setting up AoE</h2>
        <span className="text-sm text-text-secondary">
          {done} of {steps.length} done. Nothing here blocks you.
        </span>
        <Link to="/setup" className="ml-auto inline-flex items-center gap-1 text-sm text-brand-500 hover:underline">
          Resume setup <ArrowRight size={12} />
        </Link>
        <button
          type="button"
          aria-label="Dismiss setup checklist"
          onClick={onDismiss}
          className="h-8 w-8 -mr-2 flex items-center justify-center text-text-dim hover:text-text-primary cursor-pointer"
        >
          <X size={14} />
        </button>
      </div>
      <div className="mx-4 my-3 h-1 rounded-full bg-surface-700" aria-hidden="true">
        <div className="h-1 rounded-full bg-brand-600" style={{ width: `${percent}%` }} />
      </div>
      <ul className="divide-y divide-surface-700 border-t border-surface-700">
        {open.map((step) => {
          const info = SETUP_STEP_INFO[step.key];
          return (
            <li key={step.key} className="flex items-center gap-4 px-4 min-h-8 py-1.5 text-sm">
              <span className="text-text-primary w-48">{info.title}</span>
              <span className="text-text-dim">{step.skipped ? "Skipped" : "Not set up"}</span>
              <Link to={info.link} className="ml-auto text-text-secondary hover:text-text-primary">
                {info.linkLabel}
              </Link>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
