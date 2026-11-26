import { DIFF_SCOPES, NO_LAST_TURN, useDiffScopeContext } from "../../lib/diffScope";
import { ACTIVE, GROUP, IDLE, SEGMENT } from "./viewerChrome";

/** Branch, Uncommitted or Last turn; hidden where the scope can't change. */
export function DiffScopePicker() {
  const { scope, lastTurnAt, onChange } = useDiffScopeContext();
  if (!onChange) return null;
  return (
    <div role="radiogroup" aria-label="Compare" className={GROUP} data-testid="diff-scope-picker">
      {DIFF_SCOPES.map((s) => {
        const unavailable = s.scope === "last_turn" && !lastTurnAt;
        const title = unavailable
          ? NO_LAST_TURN
          : s.scope === "last_turn"
            ? `${s.title} (${new Date(lastTurnAt!).toLocaleTimeString()})`
            : s.title;
        return (
          <button
            key={s.scope}
            type="button"
            role="radio"
            aria-checked={scope === s.scope}
            disabled={unavailable}
            title={title}
            data-testid={`diff-scope-${s.scope}`}
            onClick={() => onChange(s.scope)}
            className={
              unavailable
                ? `${SEGMENT.replace("cursor-pointer", "cursor-not-allowed")} text-text-dim/50`
                : `${SEGMENT} ${scope === s.scope ? ACTIVE : IDLE}`
            }
          >
            {s.label}
          </button>
        );
      })}
    </div>
  );
}
