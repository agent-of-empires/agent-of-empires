import type { EffectiveHookGroup, HookSource } from "../../lib/profileHooks";

interface Props {
  groups: EffectiveHookGroup[];
}

const SOURCE_BADGE: Record<HookSource, { label: string; className: string }> = {
  override: {
    label: "Profile override",
    className: "bg-brand-600/15 text-brand-400",
  },
  "override-empty": {
    label: "Overridden: none",
    className: "bg-surface-700 text-text-dim",
  },
  inherited: {
    label: "Inherited from global",
    className: "bg-surface-700 text-text-secondary",
  },
  none: {
    label: "None",
    className: "bg-surface-700 text-text-dim",
  },
};

/** Read-only view of a profile's effective lifecycle hooks.
 *
 *  Lifecycle hooks run arbitrary shell commands on session create/launch/
 *  destroy, so a hooks section set through the API would be remote code
 *  execution. The server blocks hook writes (ALLOWED_PROFILE_SETTINGS_
 *  SECTIONS in src/server/api/mod.rs) and this panel deliberately renders
 *  display-only: it takes no onChange/save props and exposes no inputs, so
 *  there is no path from here to a profile PATCH. */
export function HooksReadOnlyPanel({ groups }: Props) {
  return (
    <section className="rounded-lg border border-surface-700 bg-surface-900 p-4">
      <h3 className="text-sm font-semibold text-text-primary">Lifecycle hooks</h3>
      <p className="mt-1 text-xs text-text-dim">
        Lifecycle hooks run shell commands when sessions are created, launched, and destroyed. To prevent remote code
        execution, the dashboard shows them read-only; edit hooks in your config file or the TUI settings.
      </p>
      <ul className="mt-3 flex flex-col gap-3">
        {groups.map((group) => {
          const badge = SOURCE_BADGE[group.source];
          return (
            <li key={group.key}>
              <div className="flex items-center gap-2">
                <span className="text-xs font-medium text-text-secondary">{group.label}</span>
                <span className={`rounded-md px-1.5 py-0.5 text-[11px] font-medium ${badge.className}`}>
                  {badge.label}
                </span>
              </div>
              {group.commands.length > 0 ? (
                <ul className="mt-1 flex flex-col gap-1">
                  {group.commands.map((cmd, i) => (
                    <li
                      key={i}
                      className="rounded-md bg-surface-850 px-2 py-1 font-mono text-xs text-text-primary break-all"
                    >
                      {cmd}
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="mt-1 text-xs text-text-dim italic">No commands.</p>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
