import type { SandboxImageStatus } from "../../lib/api";

export const PULL_LATEST_HINT =
  "Your image is out of date; Structured view Sessions may stop with an adapter error without it.";
export const PULL_GATED_HINT = "Run this on the Daemon's machine, then reopen this dialog.";

export interface PullLatestOffer {
  /** Unchecked once the user has dismissed this update. */
  checkedByDefault: boolean;
  /** Set when the Daemon won't pull from the web: what to run on its machine instead. */
  command: string | null;
}

/** "Pull latest image first" is offered for a Structured view Sandbox whose image has an update. */
export function pullLatestOffer(
  reply: SandboxImageStatus | { error: string } | null | undefined,
  { sandbox, structured }: { sandbox: boolean; structured: boolean },
): PullLatestOffer | null {
  if (!sandbox || !structured || !reply || !("state" in reply) || reply.state !== "available") return null;
  return { checkedByDefault: !reply.dismissed, command: reply.pull_blocked ? reply.pull_command : null };
}

/** Whether Launch asks for the pull: never while gated, else the user's choice or the default. */
export function pullLatestChecked(offer: PullLatestOffer | null, choice: boolean | null): boolean {
  return !!offer && !offer.command && (choice ?? offer.checkedByDefault);
}
