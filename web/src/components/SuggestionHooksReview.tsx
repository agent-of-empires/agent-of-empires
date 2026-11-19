import type { useSuggestions } from "../hooks/useSuggestions";
import { HooksTrustDialog } from "./session-wizard/HooksTrustDialog";

/** The hooks review a one-click Start pauses on; every view that can Start renders it. */
export function SuggestionHooksReview({ ctrl }: { ctrl: ReturnType<typeof useSuggestions> }) {
  const pending = ctrl.hooksReview;
  if (!pending) return null;
  const { review, item } = pending;
  return (
    <HooksTrustDialog
      starting={item.title}
      onCreate={review.commands.on_create ?? []}
      onLaunch={review.commands.on_launch ?? []}
      onDestroy={review.commands.on_destroy ?? []}
      needsMcpTrust={review.needs_mcp_trust === true}
      mcpSummaries={[]}
      onConfirm={ctrl.approveHooks}
      onCancel={ctrl.cancelHooks}
    />
  );
}
