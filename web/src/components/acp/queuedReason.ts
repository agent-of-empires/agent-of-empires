import { isClearAlias } from "../../lib/agentProfiles";
import type { RateLimitInfo } from "../../lib/acpTypes";

/** What a queued row's action does, given why the queue is waiting. */
export type QueuedAction =
  /** Send now: the queue isn't held by anything this row can end. */
  | "send"
  /** Stop the running turn (it can't be steered); the Daemon then drains the queue in order. */
  | "stopAndSend"
  /** Stop the running turn so a command that must run as its own turn can run. */
  | "stopAndRun"
  /** Start the Session; the Daemon then sends the queue. */
  | "startAndSend"
  /** Nothing to do but wait; `disabledReason` says why. */
  | "disabled"
  /** The Daemon doesn't have the message yet, so it can only be dropped. */
  | "dropOnly";

export interface QueuedReason {
  /** The uppercase header label, before the count. */
  label: "Queued" | "Not sent";
  /** Sentence-case reason after the label, or null when nothing holds the queue. */
  reason: string | null;
  /** What a message row's action does. */
  action: QueuedAction;
  /** What a solo command row's action does: one can't steer into a running turn. */
  soloAction: QueuedAction;
  /** Why Send now is unavailable, for its tooltip and accessible name. */
  disabledReason?: string;
}

export interface QueuedReasonInput {
  /** The Session's Agent by display name. */
  agent: string;
  /** This dashboard's connection to the Daemon is open. */
  online: boolean;
  /** A row hasn't reached the Daemon yet. */
  anyPending: boolean;
  /** A Session that has never started: the Agent it waits for. */
  waitingFor: string | null;
  rateLimitParked: boolean;
  rateLimit: Pick<RateLimitInfo, "resets_at" | "status"> | null;
  /** The profile's `rate_limit_auto_resume`; unknown until the Session reports it. */
  rateLimitAutoResume: boolean | undefined;
  /** No live Worker, none starting, and nothing a send would wake. */
  workerDown: boolean;
  /** A Worker is starting or restarting; it sends the queue once it's up. */
  workerStarting: boolean;
  cancelling: boolean;
  compacting: boolean;
  turnActive: boolean;
  steerable: boolean;
  /** The first queued row's text, to tell a solo command. */
  firstText: string | undefined;
  /** Clear and compact aliases: commands that run as their own turn. */
  soloAliases: readonly string[];
}

function resetTime(resetsAt: string): string | null {
  const at = new Date(resetsAt);
  return Number.isNaN(at.getTime()) ? null : at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** Why the queue is waiting, in the order the states outrank each other. */
export function queuedReason(input: QueuedReasonInput): QueuedReason {
  const { agent } = input;
  const same = (label: QueuedReason["label"], reason: string | null, action: QueuedAction): QueuedReason => ({
    label,
    reason,
    action,
    soloAction: action,
  });
  const waiting = (reason: string): QueuedReason => ({
    ...same("Queued", reason, "disabled"),
    disabledReason: `Waiting: this message ${reason}`,
  });
  if (!input.online && input.anyPending) {
    return same("Not sent", "sends when this dashboard reconnects", "dropOnly");
  }
  if (input.waitingFor) return same("Queued", `sends when ${input.waitingFor} is ready`, "send");
  if (input.rateLimitParked) {
    // Only an armed auto-resume sends by itself; otherwise the queue waits for the user. Running
    // out of retries stops re-sending the interrupted prompt, not queued messages: the Daemon
    // still releases the park for them, so it doesn't change this.
    if (input.rateLimitAutoResume === false) return same("Queued", "held until you resume: auto-resume is off", "send");
    if (input.rateLimitAutoResume === undefined) return same("Queued", "held by the rate limit", "send");
    const at = input.rateLimit?.resets_at ? resetTime(input.rateLimit.resets_at) : null;
    return same(
      "Queued",
      at ? `sends after the rate limit resets at ${at}` : "sends after the rate limit lifts",
      "send",
    );
  }
  if (input.workerStarting) return waiting(`sends when ${agent} starts`);
  if (input.workerDown) return same("Queued", `sends when ${agent} starts`, "startAndSend");
  if (input.cancelling) return waiting(`sends once ${agent} stops`);
  if (input.compacting) return waiting("sends after compacting");
  if (input.turnActive) {
    // A steerable turn takes a message now; a solo command always needs the turn stopped.
    const action = input.steerable ? "send" : "stopAndSend";
    const first = input.firstText?.trim() ?? "";
    const reason =
      first && isClearAlias(first, input.soloAliases)
        ? `${first.split(/\s/)[0]} runs when this turn ends`
        : input.steerable
          ? null
          : `sends when ${agent} finishes`;
    return { label: "Queued", reason, action, soloAction: "stopAndRun" };
  }
  return same("Queued", null, "send");
}
