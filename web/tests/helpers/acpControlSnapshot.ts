import type { AcpConnectionUnavailability, AcpState, ReducedState } from "../../src/lib/acpTypes";

/** A fixture's declared current receipt, deliberately independent of replay/SIDs/socket-open. */
export function currentControlSnapshot(
  state: AcpState,
  receipt: { ready: boolean; unavailable?: AcpConnectionUnavailability | null; wakeEligible?: boolean },
  seq = state.lastSeq,
): { kind: "reduced_state"; seq: number; state: ReducedState; unchanged: string[] } {
  const unavailable = receipt.unavailable ?? null;
  return {
    kind: "reduced_state",
    seq,
    unchanged: [],
    state: {
      agent: state.agent ?? "claude",
      model: state.model,
      mode: state.mode,
      current_plan: state.plan,
      in_flight_tool: unavailable ? null : state.inFlightTool,
      pending_approvals: unavailable ? [] : state.pendingApprovals,
      pending_elicitations: unavailable ? [] : state.pendingElicitations,
      thinking: !unavailable && state.thinking ? { started_at: "2026-01-01T00:00:00Z" } : null,
      rate_limit: state.rateLimit,
      available_commands: state.availableCommands,
      available_modes: state.availableModes,
      current_mode_id: state.currentModeId,
      auth_status: state.authStatus,
      turn_active: !unavailable && state.serverTurnActive,
      cancelling: !unavailable && state.cancelling,
      compacting: !unavailable && state.compacting,
      session_notices: state.sessionNotices,
      background_tasks: state.backgroundTasks,
      operation_error: state.lastErrorEvidence,
      connection_unavailable: unavailable,
      connection_ready: receipt.ready && unavailable === null,
      prompt_wake_eligible: receipt.wakeEligible === true && unavailable === null,
      last_seq: state.lastSeq,
    },
  };
}
