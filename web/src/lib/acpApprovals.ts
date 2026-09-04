import type { Approval } from "./acpTypes";

/** Whether the request's options are a question's choices rather than an
 *  allow/deny vocabulary: more than two of one kind, or pi's
 *  `ask_user_question` tool-call id prefix. Mirrors
 *  `Approval::is_choice_list` in src/acp/approvals.rs. Such a card lists the
 *  labels and resolves with the chosen option; the fixed trio would answer
 *  with whichever option came first (#3741). */
export function isChoiceList(approval: Pick<Approval, "options" | "tool_call">): boolean {
  const options = approval.options ?? [];
  if (approval.tool_call.id.startsWith("pi-ui-") && options.length > 0) return true;
  return options.length > 2 && options.every((o) => o.kind === options[0]!.kind);
}
