import { useApprovalSound } from "../../hooks/useApprovalSound";

interface Props {
  /** Number of pending tool approvals. */
  approvals: number;
  /** Number of pending AskUserQuestion elicitations. */
  elicitations: number;
}

// Browser-side attention chime, extracted from StructuredView so the wiring
// is unit-mountable without the assistant-ui runtime (the #1282 pattern used
// for StructuredViewRoot / RateLimitRecoverySection). Fires once on the
// 0 -> >=1 edge of the combined pending approvals + questions count;
// complements the OS push (delivered via the SW when the dashboard is
// backgrounded) and the in-app toast (when foregrounded). A question
// arriving while an approval is already pending does not re-chime, but its
// OS push still fires on the live event edge regardless. See #1038, #2146.
export function AttentionChime({ approvals, elicitations }: Props): null {
  useApprovalSound(approvals + elicitations);
  return null;
}
