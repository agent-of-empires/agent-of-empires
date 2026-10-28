import { isClearAlias } from "../../lib/agentProfiles";
import type { RateLimitInfo } from "../../lib/acpTypes";

/** What a queued row's action does, given why the queue is waiting. */
export type QueuedAction =
  /** Send now: the queue isn't held by anything this row can end. */
  | "send"
  /** Stop the running turn so this message goes next. */
  | "stopAndSend"
  /** As stopAndSend, for a command that must run as its own turn. */
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
  action: QueuedAction;
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
  if (!input.online && input.anyPending) {
    return { label: "Not sent", reason: "sends when this dashboard reconnects", action: "dropOnly" };
  }
  if (input.waitingFor) {
    return { label: "Queued", reason: `sends when ${input.waitingFor} is ready`, action: "send" };
  }
  if (input.rateLimitParked) {
    const at = input.rateLimit?.resets_at ? resetTime(input.rateLimit.resets_at) : null;
    return {
      label: "Queued",
      reason: at ? `sends after the rate limit resets at ${at}` : "sends after the rate limit lifts",
      action: "send",
    };
  }
  if (input.workerStarting) {
    const reason = `sends when ${agent} starts`;
    return { label: "Queued", reason, action: "disabled", disabledReason: `Waiting: this ${reason}` };
  }
  if (input.workerDown) {
    return { label: "Queued", reason: `sends when ${agent} starts`, action: "startAndSend" };
  }
  if (input.cancelling) {
    const reason = `sends once ${agent} stops`;
    return { label: "Queued", reason, action: "disabled", disabledReason: `Waiting: this ${reason}` };
  }
  if (input.compacting) {
    const reason = "sends after compacting";
    return { label: "Queued", reason, action: "disabled", disabledReason: `Waiting: this ${reason}` };
  }
  if (input.turnActive) {
    const first = input.firstText?.trim() ?? "";
    if (first && isClearAlias(first, input.soloAliases)) {
      return { label: "Queued", reason: `${first.split(/\s/)[0]} runs when this turn ends`, action: "stopAndRun" };
    }
    if (!input.steerable) {
      return { label: "Queued", reason: `sends when ${agent} finishes`, action: "stopAndSend" };
    }
  }
  return { label: "Queued", reason: null, action: "send" };
}
