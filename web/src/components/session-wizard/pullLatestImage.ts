import type { SandboxImageStatus } from "../../lib/api";
import { pullProgress, type SandboxPullProgress } from "../../hooks/useSandboxImageBanner";

export const PULL_LATEST_HINT =
  "Your image is out of date; Structured view Sessions may stop with an adapter error without it.";
export const PULL_GATED_HINT = "Run this on the Daemon's machine, then reopen this dialog.";

export interface PullLatestOffer {
  /** Unchecked once the user has dismissed this update. */
  checkedByDefault: boolean;
  /** Set when the Daemon won't pull from the web: what to run on its machine instead. */
  command: string | null;
  /** Set while a pull of this image runs: Launch can wait for it or start on the current image. */
  running: SandboxPullProgress | null;
}

/** "Pull latest image first" is offered for a Structured view Sandbox whose image has an update, or
 *  whose image is downloading now, so a create can wait for that download instead of starting another. */
export function pullLatestOffer(
  reply: SandboxImageStatus | { error: string } | null | undefined,
  { sandbox, structured }: { sandbox: boolean; structured: boolean },
): PullLatestOffer | null {
  if (!sandbox || !structured || !reply || !("state" in reply)) return null;
  const command = reply.pull_blocked ? reply.pull_command : null;
  // Gated, the dashboard can't join a pull either, so it keeps the gated checkbox.
  const running = reply.pull.state === "running" && !command ? pullProgress(reply.pull) : null;
  // A first pull leaves no current image to start on, and the create joins that download anyway.
  if (running && reply.state === "unknown" && reply.reason === "not_pulled") return null;
  if (!running && reply.state !== "available") return null;
  // Waiting is the default, as pulling first is: the create gets the image the download brings.
  return { checkedByDefault: running ? true : !reply.dismissed, command, running };
}

/** Whether Launch asks for the pull: never while gated, else the user's choice or the default. A create
 *  that asks for it while a download runs joins that download. */
export function pullLatestChecked(offer: PullLatestOffer | null, choice: boolean | null): boolean {
  return !!offer && !offer.command && (choice ?? offer.checkedByDefault);
}
