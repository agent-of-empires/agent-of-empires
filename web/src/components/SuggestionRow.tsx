import type { ReactNode } from "react";

interface Props {
  title: string;
  source: string;
  reason: string;
  project?: string | null;
  actions: ReactNode;
}

export function SuggestionRow({ title, source, reason, project, actions }: Props) {
  return (
    <li className="flex items-center gap-3 border-t border-surface-700/40 px-3 py-2.5 first:border-t-0">
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm text-text-primary">{title}</p>
        <p className="mt-0.5 truncate text-xs text-text-muted">
          <span className="font-mono font-medium text-text-secondary">{source}</span>
          {" · "}
          {reason}
          {project && (
            <>
              {" · "}
              <span className="font-mono font-medium text-text-secondary">{project}</span>
            </>
          )}
        </p>
      </div>
      <div className="flex flex-none gap-1">{actions}</div>
    </li>
  );
}
