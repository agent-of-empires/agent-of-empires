import type { CreateSessionRequest } from "../../lib/types";
import type { WizardData } from "./wizardReducer";

/** The create payload for the wizard state. Scratch omits every worktree field
 *  (the server rejects scratch plus a worktree branch); the server re-validates
 *  structured view capability. */
export function buildCreateRequest(d: WizardData, acpCapable: boolean): CreateSessionRequest {
  const worktree = !d.scratch && d.useWorktree;
  const newBranch = worktree && !d.attachExisting;
  const structured = acpCapable && d.useStructuredView;
  return {
    path: d.scratch ? "" : d.path,
    tool: d.tool,
    title: d.title || undefined,
    group: d.group || undefined,
    yolo_mode: d.yoloMode,
    worktree_enabled: worktree,
    worktree_branch: worktree && d.worktreeBranchDirty && d.worktreeBranch.trim() ? d.worktreeBranch.trim() : undefined,
    create_new_branch: newBranch,
    base_branch: newBranch && d.baseBranch.trim() ? d.baseBranch.trim() : undefined,
    sandbox: d.sandboxEnabled,
    sandbox_image: d.sandboxEnabled ? d.sandboxImage : undefined,
    extra_env: d.sandboxEnabled && d.extraEnv.length > 0 ? d.extraEnv.filter(Boolean) : undefined,
    extra_repo_paths: !d.scratch && d.extraRepoPaths.length > 0 ? d.extraRepoPaths : undefined,
    // A base is meaningless when attaching to an existing branch.
    repo_bases: newBranch
      ? d.extraRepoPaths
          .map((p) => ({ repo: p, base_branch: (d.repoBases[p] ?? "").trim() }))
          .filter((r) => r.base_branch)
      : undefined,
    extra_args: d.extraArgs || undefined,
    command_override: d.commandOverride || undefined,
    custom_instruction: d.customInstruction || undefined,
    profile: d.profile || undefined,
    view: structured ? "structured" : "terminal",
    agent_model: structured && d.agentModel ? d.agentModel : undefined,
    agent_effort: structured && d.agentEffort ? d.agentEffort : undefined,
    scratch: d.scratch || undefined,
    import_acp_session_id: d.importAcpSessionId || undefined,
  };
}
