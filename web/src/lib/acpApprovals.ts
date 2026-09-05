import type { Approval } from "./acpTypes";

/** Whether the request is a question whose options are its choices, as
 *  decided by the daemon (`Approval.choice_list`, src/acp/approvals.rs) so
 *  every client renders the same card. Such a card lists the labels and
 *  resolves with the chosen option; the fixed trio would answer with
 *  whichever option came first (#3741). */
export function isChoiceList(approval: Pick<Approval, "choice_list" | "options">): boolean {
  return approval.choice_list === true && (approval.options?.length ?? 0) > 0;
}
