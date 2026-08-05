import { isCompactionReminderDue, type AcpState } from "../../lib/acpTypes";
import { useAcpPrefs } from "../../lib/acpPrefs";

/**
 * Banner shown above the structured view composer once the agent's context
 * window passes the configured percentage, offering to run `/compact`. Opt
 * in through `[acp] compaction_reminder`; off by default. See #3253.
 *
 * Unlike the composer's usage chip this renders at every viewport, since
 * mobile is where the chip is hidden (`hidden sm:inline-flex`) and so where
 * a filling window is otherwise invisible.
 *
 * "Compact now" sends the command instead of prefilling the composer:
 * prefill goes through `composerRuntime.setText`, which would silently
 * destroy a draft the user had typed. Losing typed work is worse than a
 * labelled button doing what it says, and compaction leaves the transcript
 * and the event log untouched.
 *
 * Dismiss state lives on the reducer (`dismissCompactionReminder`) rather
 * than component `useState`, so dismissing once survives a session switch,
 * and re-arms on the next context boundary.
 */
interface Props {
  state: Pick<AcpState, "sessionUsage" | "compacting" | "compactionReminderDismissed" | "availableCommands">;
  onCompact: () => void;
  onDismiss: () => void;
}

export function CompactionReminderBanner({ state, onCompact, onDismiss }: Props) {
  const prefs = useAcpPrefs();
  if (!isCompactionReminderDue(state, prefs)) return null;
  const usage = state.sessionUsage;
  if (!usage) return null;
  const pct = Math.round((usage.used / usage.size) * 100);

  return (
    <div
      role="status"
      data-testid="compaction-reminder"
      className="bg-amber-900/30 border-y border-amber-700/40 px-4 py-2 flex items-center gap-3 text-xs font-mono text-amber-200"
    >
      <span className="shrink-0 text-amber-400" aria-hidden="true">
        ⚠
      </span>
      <span className="flex-1 leading-snug">
        Context window {pct}% full.{" "}
        <span className="text-amber-100/70">
          Compacting replaces the earlier turns in the model&apos;s context with a summary.
        </span>
      </span>
      <button
        type="button"
        onClick={onCompact}
        className="shrink-0 px-2 py-1 rounded bg-amber-800/40 hover:bg-amber-700/50 border border-amber-700/60 text-amber-100 cursor-pointer transition-colors"
      >
        Compact now
      </button>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss compaction reminder"
        className="shrink-0 px-1 text-amber-300/70 hover:text-amber-100 cursor-pointer"
      >
        &times;
      </button>
    </div>
  );
}
