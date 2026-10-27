import type { CreateFailure, CreateStage } from "../../lib/types";

export type FailureAction =
  | "retry_runtime"
  | "launch_without_sandbox"
  | "change_image"
  | "choose_project"
  | "try_again";

export const FAILURE_ACTION_LABEL: Record<FailureAction, string> = {
  retry_runtime: "Retry",
  launch_without_sandbox: "Launch without Sandbox",
  change_image: "Change image",
  choose_project: "Choose Project",
  try_again: "Try again",
};

export interface FailureView {
  title: string;
  /** The failed step as one sentence; null when the server didn't say. */
  step: string | null;
  /** The server's message, unless it only repeats the title. */
  detail: string | null;
  primary: FailureAction;
  secondary: FailureAction | null;
  /** The row that caused it, outlined, with `cue` added to its summary. */
  row: "sandbox" | "project" | null;
  cue: string | null;
  /** The last create-progress output lines belong in the box. */
  showOutput: boolean;
}

const STEP: Record<CreateStage, string> = {
  preparing: "It failed while preparing the Session.",
  starting_container: "It failed while starting the container.",
  running_hooks: "It failed while running the on_create hooks.",
  starting: "It failed while starting the Agent.",
};

const GENERIC_MESSAGE = "Failed to create session";

/** What the wizard shows for a failed create. `runtimeDown`: a Retry re-check found the
 *  Container runtime still down. */
export function failureView(
  failure: CreateFailure,
  { sandboxEnabled, runtimeDown }: { sandboxEnabled: boolean; runtimeDown: boolean },
): FailureView {
  const base = {
    step: failure.stage ? STEP[failure.stage] : null,
    detail: failure.message && failure.message !== GENERIC_MESSAGE ? failure.message : null,
    showOutput: false,
  };
  // Only a Sandbox that is on can be turned off.
  const withoutSandbox = sandboxEnabled ? ("launch_without_sandbox" as const) : null;
  switch (failure.code) {
    case "sandbox_unavailable":
      return {
        ...base,
        title: "The Sandbox couldn't start",
        primary: runtimeDown && withoutSandbox ? withoutSandbox : "retry_runtime",
        secondary: runtimeDown && withoutSandbox ? "retry_runtime" : withoutSandbox,
        row: "sandbox",
        cue: "couldn't start",
      };
    case "sandbox_image_pull_failed":
      return {
        ...base,
        title: "The Sandbox image couldn't be pulled",
        primary: "change_image",
        secondary: withoutSandbox,
        row: "sandbox",
        cue: "image pull failed",
      };
    case "hook_failed":
      // In a Sandbox the Repo hooks run inside the container, so turning it off is no fix.
      return {
        ...base,
        title: "An on_create hook failed",
        primary: "try_again",
        secondary: null,
        row: null,
        cue: null,
        showOutput: true,
      };
    case "working_dir_missing":
      return {
        ...base,
        title: "The Working directory is missing",
        primary: "choose_project",
        secondary: null,
        row: "project",
        cue: "not found where the Daemon runs",
      };
    default: {
      const inContainer = failure.code === "create_failed" && failure.stage === "starting_container";
      return {
        ...base,
        title: "The Session couldn't be created",
        primary: "try_again",
        secondary: inContainer ? withoutSandbox : null,
        row: inContainer && withoutSandbox ? "sandbox" : null,
        cue: inContainer && withoutSandbox ? "couldn't start" : null,
      };
    }
  }
}
