export interface RecoveryAction {
  label: string;
  onClick: () => void;
  testId: string;
  disabled?: boolean;
  title?: string;
}

const BUTTON =
  "min-h-8 rounded-md px-3 py-1.5 text-xs font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-60";

/** Label and fill are a theme-projected readable pair; hover adds a ring instead of
 *  changing either, so the pair holds in every state. */
const PRIMARY = `${BUTTON} bg-status-error text-text-on-status-error hover:ring-2 hover:ring-status-error/50`;

/** The recovery buttons both startup error surfaces show, primary first, and the
 *  install's failure and npm output under them. */
export function RecoveryActionRow({
  primary,
  secondary,
  install,
}: {
  primary: RecoveryAction;
  secondary: RecoveryAction[];
  install: { error: string | null; output: string | null };
}) {
  return (
    <>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        {[primary, ...secondary].map((action) => (
          <button
            key={action.testId}
            type="button"
            data-testid={action.testId}
            onClick={action.onClick}
            disabled={action.disabled}
            title={action.title}
            className={
              action === primary
                ? `${PRIMARY} w-full sm:w-auto`
                : `${BUTTON} border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700`
            }
          >
            {action.label}
          </button>
        ))}
      </div>
      {install.error && (
        <div className="mt-2 text-xs text-status-error-text" data-testid="adapter-install-error">
          {install.error}
        </div>
      )}
      {install.output && (
        <pre className="mt-2 max-h-40 overflow-auto rounded-md border border-surface-700 bg-surface-950 p-2 font-mono text-[11px] text-text-subtle">
          {install.output}
        </pre>
      )}
    </>
  );
}
