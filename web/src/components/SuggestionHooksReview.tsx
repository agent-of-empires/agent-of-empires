import { suggestionKey, type useSuggestions } from "../hooks/useSuggestions";
import { HooksTrustDialog } from "./session-wizard/HooksTrustDialog";

/** The hooks review a one-click Start pauses on; every view that can Start renders it. */
export function SuggestionHooksReview({ ctrl }: { ctrl: ReturnType<typeof useSuggestions> }) {
  const pending = ctrl.hooksReview;
  if (!pending) return null;
  const { review, item } = pending;
  return (
    <HooksTrustDialog
      // A fresh dialog per review: the next one must not inherit the last one's in-flight state.
      key={`${suggestionKey(item)}:${review.review_fingerprint}`}
      starting={item.title}
      onCreate={review.commands.on_create ?? []}
      onLaunch={review.commands.on_launch ?? []}
      onDestroy={review.commands.on_destroy ?? []}
      needsMcpTrust={review.needs_mcp_trust === true}
      mcpSummaries={review.mcp_summaries ?? []}
      onConfirm={ctrl.approveHooks}
      onCancel={ctrl.cancelHooks}
    />
  );
}
