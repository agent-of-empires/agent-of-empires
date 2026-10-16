import { useApprovalSound } from "../../hooks/useApprovalSound";

interface Props {
  sessionId: string;
  approvals: number;
  elicitations: number;
}

/** Chimes once when pending approvals plus questions go from zero to some. */
export function AttentionChime({ sessionId, approvals, elicitations }: Props): null {
  useApprovalSound(approvals + elicitations, sessionId);
  return null;
}
