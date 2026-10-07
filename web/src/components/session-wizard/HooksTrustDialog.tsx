import { ConfirmCreateDialog } from "./ConfirmCreateDialog";

interface Props {
  onCreate: string[];
  onLaunch: string[];
  onDestroy: string[];
  needsMcpTrust: boolean;
  onConfirm: () => Promise<void> | void;
  onCancel: () => void;
}

/** Shown when a create is refused until the repo's hooks are trusted; confirming resubmits with `trust_hooks`. */
export function HooksTrustDialog({ onCreate, onLaunch, onDestroy, needsMcpTrust, onConfirm, onCancel }: Props) {
  // Approval trusts the whole hooks hash, so every covered hook type is listed.
  const groups = [
    { name: "on_create", commands: onCreate },
    { name: "on_launch", commands: onLaunch },
    { name: "on_destroy", commands: onDestroy },
  ].filter((g) => g.commands.length > 0);

  return (
    <ConfirmCreateDialog
      id="hooks-trust"
      title="Trust repository hooks"
      confirmLabel="Trust and create"
      onConfirm={onConfirm}
      onCancel={onCancel}
    >
      <p className="text-[13px] text-text-secondary">
        This repository defines lifecycle hooks. They run on your machine; approving trusts every hook type listed
        below, not only the ones that run now. Review the commands before approving.
      </p>

      <div className="space-y-2 max-h-40 overflow-y-auto" data-testid="hooks-trust-list">
        {groups.map((group) => (
          <div key={group.name}>
            <div className="text-[11px] font-mono text-text-dim">{group.name}:</div>
            <ul className="space-y-1">
              {group.commands.map((cmd, i) => (
                <li key={i} className="text-[13px] font-mono text-text-primary break-all pl-3">
                  {cmd}
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>

      {needsMcpTrust && (
        <p className="text-[12px] text-text-dim">
          The repository's <span className="font-mono text-text-secondary">.mcp.json</span> will also be trusted.
        </p>
      )}

      <p className="text-[12px] text-text-dim">
        Approving trusts this repository's hooks so future sessions (including worktrees) run them without prompting.
      </p>
    </ConfirmCreateDialog>
  );
}
