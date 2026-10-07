import { useApprovalSound } from "../../hooks/useApprovalSound";

interface Props {
  approvals: number;
  elicitations: number;
}

/** Chimes once when pending approvals plus questions go from zero to some. */
export function AttentionChime({ approvals, elicitations }: Props): null {
  useApprovalSound(approvals + elicitations);
  return null;
}
