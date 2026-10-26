import { useId, useState, type ReactNode } from "react";
import type { SettingsIntroId } from "../../lib/settingsIntros";

// Copy explains what a tab is for and never states a setting's current value.
const INTROS: Record<SettingsIntroId, { summary: ReactNode; details: string }> = {
  worktree: {
    summary:
      "Worktrees give each session its own copy of the branch, so two agents in the same repository never overwrite each other's files.",
    details:
      "One git worktree per session. The path template expands {repo-name}, {branch} and {session-id}. The bare-repo template is under Advanced.",
  },
  plugins: {
    summary: (
      <>
        Plugins add features to AoE. Browse them under Marketplace: <strong>Featured</strong> ones are reviewed by the
        maintainers, unvetted ones aren't.
      </>
    ),
    details:
      "Marketplace lists GitHub repositories tagged aoe-plugin. Install shows the capabilities a plugin asks for before anything runs. Featured means reviewed and pinned to a release.",
  },
  "structured-view": {
    summary: "Choose the model, mode and thinking level each agent starts with, so you don't set them every time.",
    details:
      "Applies to Structured view sessions. The options come from what each agent last advertised, so new models appear automatically. Thinking level can be set per model.",
  },
};

export function SettingsIntro({ id, onFold }: { id: SettingsIntroId; onFold: () => void }) {
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  const { summary, details } = INTROS[id];
  return (
    <section
      aria-label="About this tab"
      className="rounded-lg border border-surface-700 bg-surface-850 px-4 py-3 text-sm leading-relaxed text-text-primary"
    >
      <p>{summary}</p>
      <p
        id={detailsId}
        hidden={!open}
        className="mt-3 border-t border-surface-700/60 pt-3 font-mono text-xs text-text-subtle"
      >
        {details}
      </p>
      <div className="mt-2 flex flex-wrap gap-x-5">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((o) => !o)}
          className="min-h-8 cursor-pointer font-medium text-text-primary underline-offset-4 hover:underline"
        >
          Details <span aria-hidden="true">{open ? "⌄" : "›"}</span>
        </button>
        <button
          type="button"
          onClick={onFold}
          className="min-h-8 cursor-pointer font-medium text-text-primary underline-offset-4 hover:underline"
        >
          Got it
        </button>
      </div>
    </section>
  );
}

export function SettingsIntroReopen({ onReopen }: { onReopen: () => void }) {
  return (
    <button
      type="button"
      onClick={onReopen}
      aria-label="About this tab"
      className="inline-flex min-h-8 cursor-pointer items-center gap-1 text-xs text-text-subtle hover:text-text-primary"
    >
      <span aria-hidden="true">ⓘ</span>
      About
    </button>
  );
}
