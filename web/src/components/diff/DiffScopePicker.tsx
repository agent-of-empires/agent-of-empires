import { DIFF_SCOPES, useDiffScopeContext } from "../../lib/diffScope";
import { ACTIVE, GROUP, IDLE, SEGMENT } from "./viewerChrome";

/** Branch or Uncommitted; hidden where the scope can't change. */
export function DiffScopePicker() {
  const { scope, onChange } = useDiffScopeContext();
  if (!onChange) return null;
  return (
    <div role="radiogroup" aria-label="Compare" className={GROUP} data-testid="diff-scope-picker">
      {DIFF_SCOPES.map((s) => {
        return (
          <button
            key={s.scope}
            type="button"
            role="radio"
            aria-checked={scope === s.scope}
            title={s.title}
            data-testid={`diff-scope-${s.scope}`}
            onClick={() => onChange(s.scope)}
            className={`${SEGMENT} ${scope === s.scope ? ACTIVE : IDLE}`}
          >
            {s.label}
          </button>
        );
      })}
    </div>
  );
}
