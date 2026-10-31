import type { SuggestionPrepared } from "./types";
import type { WizardPrefill } from "../components/session-wizard/SessionWizard";

export const STALE_NOTICE = "That suggestion changed. Refreshing the list.";

// `p.worktree` is dropped on purpose: the wizard resolves worktree itself, and
// `WizardPrefill.worktreeEnabled` means a registered-project override.
export function toWizardPrefill(
  p: SuggestionPrepared,
  source: NonNullable<WizardPrefill["source"]>,
  suggestion?: WizardPrefill["suggestion"],
): WizardPrefill {
  return {
    source,
    suggestion,
    title: p.title || undefined,
    path: p.project?.path,
    tool: p.tool,
    yoloMode: p.yolo,
    sandboxEnabled: p.sandbox,
    // A blank initial_prompt is rejected by POST /api/sessions, so send none.
    initialPrompt: p.initial_prompt || undefined,
  };
}
