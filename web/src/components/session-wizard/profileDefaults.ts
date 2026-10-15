import type { fetchSettings } from "../../lib/api";
import type { Action } from "./wizardReducer";

type Settings = NonNullable<Awaited<ReturnType<typeof fetchSettings>>>;
type Obj = Record<string, unknown> | undefined;

/** The launch knobs a profile seeds, without the dispatch bookkeeping. */
export type ProfileDefaults = Omit<Extract<Action, { type: "APPLY_PROFILE_DEFAULTS" }>, "type" | "skipIfDirty">;

/**
 * Reads a profile's settings payload into wizard defaults. `preferredTool` wins
 * over the profile's own default; `currentTool` only picks the ACP entry when
 * neither names a tool, since the reducer keeps the current tool for an empty one.
 */
export function profileDefaults(settings: Settings, preferredTool: string, currentTool: string): ProfileDefaults {
  const session = settings.session as Obj;
  const sandbox = settings.sandbox as Obj;
  const worktree = settings.worktree as Obj;
  const tool = preferredTool || (session?.default_tool as string) || "";
  const acp = (session?.acp_defaults as Obj)?.[tool || currentTool] as Obj;
  return {
    yoloMode: (session?.yolo_mode_default as boolean) ?? false,
    sandboxEnabled: (sandbox?.enabled_by_default as boolean) ?? false,
    worktreeEnabled: (worktree?.enabled as boolean) ?? false,
    tool,
    // Without the profile env, an empty extra_env makes the backend use the wrong profile's env.
    extraEnv: Array.isArray(sandbox?.environment)
      ? (sandbox.environment as unknown[]).filter((v): v is string => typeof v === "string")
      : [],
    agentModel: typeof acp?.model === "string" ? acp.model : "",
    agentEffort: typeof acp?.effort === "string" ? acp.effort : "",
    // `auto` (or unset) keeps the dashboard's own default, the structured view.
    useStructuredView: (settings.acp as Obj)?.default_new_session_view !== "terminal",
  };
}
