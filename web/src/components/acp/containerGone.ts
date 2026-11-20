import type { RecreateContainerBlock } from "../sidebar/sessionActionTargets";

/** What a container-gone Session's screen tells the user to do: only a control this dashboard offers. */
export type ContainerGoneRemedy = "recreate" | "owner" | "restore" | "unarchive" | "creating" | "deleting" | "none";

export function containerGoneRemedy(canRestart: boolean, block: RecreateContainerBlock | null): ContainerGoneRemedy {
  if (!canRestart) return "owner";
  switch (block) {
    case null:
      return "recreate";
    case "trashed":
      return "restore";
    case "archived":
      return "unarchive";
    case "creating":
    case "deleting":
      return block;
    case "not_sandboxed":
      return "none";
  }
}

/** The banner's line after "This Session's sandbox container is gone.". */
export const REMEDY_LINE: Record<Exclude<ContainerGoneRemedy, "none">, string> = {
  recreate: "Recreate it to use this Session again.",
  owner: "Ask the owner to recreate the container.",
  restore: "Restore this Session from Trash, then recreate its container.",
  unarchive: "Unarchive this Session from the sidebar, then recreate its container.",
  creating: "Wait for this Session to finish being created.",
  deleting: "Wait for this Session to finish being deleted.",
};

/** The composer's placeholder and the queued rows' reason. Archived and trashed Sessions have no composer. */
export function composerBlockedReason(remedy: ContainerGoneRemedy): string {
  switch (remedy) {
    case "recreate":
      return "Recreate the container to send messages.";
    case "none":
      return "Start a new Session to send messages.";
    default:
      return REMEDY_LINE[remedy];
  }
}
