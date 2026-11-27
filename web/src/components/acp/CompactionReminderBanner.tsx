import { isCompactionReminderDue, type AcpState } from "../../lib/acpTypes";
import { useAcpPrefs } from "../../lib/acpPrefs";
import { BUTTON_WARNING_SM } from "../../lib/surfaces";
import { WarningStrip } from "../panelParts";

/** Opt-in `/compact` reminder past the configured context usage. Shown at every width
 *  because mobile hides the usage chip. It sends the command rather than prefilling,
 *  which would overwrite a typed draft. Dismissal lives in the reducer. */
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
    <WarningStrip
      role="status"
      testId="compaction-reminder"
      title={`Context window ${pct}% full.`}
      onDismiss={onDismiss}
      dismissLabel="Dismiss compaction reminder"
      actions={
        <button type="button" onClick={onCompact} className={`${BUTTON_WARNING_SM} shrink-0`}>
          Compact now
        </button>
      }
    >
      Compacting replaces the earlier turns in the model&apos;s context with a summary.
    </WarningStrip>
  );
}
