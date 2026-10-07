import type { RichDiffFile } from "../../lib/types";
import { useWebSettings } from "../../hooks/useWebSettings";
import { LineCounts } from "./DiffFileRows";
import { ACTIVE, BackButton, GROUP, IDLE, MarkdownToggle, ToggleButton } from "./viewerChrome";

const STATUS: Record<string, [label: string, color: string]> = {
  added: ["Added", "text-status-running"],
  modified: ["Modified", "text-status-waiting"],
  deleted: ["Deleted", "text-status-error"],
  renamed: ["Renamed", "text-accent-600"],
  copied: ["Copied", "text-accent-600"],
  untracked: ["Untracked", "text-text-muted"],
  conflicted: ["Conflicted", "text-status-waiting"],
  unchanged: ["Unchanged", "text-text-muted"],
};

interface Props {
  file: RichDiffFile;
  onClose?: () => void;
  markdownAvailable: boolean;
  showRendered: boolean;
  findOpen: boolean;
  onToggleFind: () => void;
  isWide: boolean;
  splitActive: boolean;
}

export function DiffViewerHeader({
  file,
  onClose,
  markdownAvailable,
  showRendered,
  findOpen,
  onToggleFind,
  isWide,
  splitActive,
}: Props) {
  const { settings, update } = useWebSettings();
  const [label, color] = STATUS[file.status] ?? [file.status, "text-text-muted"];
  const split = settings.diffViewLayout === "split";
  return (
    <div className="px-3 py-2 border-b border-surface-700/20 flex items-center gap-2 shrink-0 flex-wrap">
      {onClose && <BackButton label="Terminal" onClick={onClose} />}
      <span className={`font-mono text-[11px] font-semibold ${color}`}>{label}</span>
      <span className="font-mono text-[12px] text-text-primary truncate">
        {file.old_path ? `${file.old_path} → ${file.path}` : file.path}
      </span>
      <LineCounts additions={file.additions} deletions={file.deletions} />
      <div className="ml-auto flex items-center gap-2">
        {markdownAvailable && <MarkdownToggle />}
        {!showRendered && (
          <>
            <button
              type="button"
              onClick={onToggleFind}
              aria-pressed={findOpen}
              title="Find in diff (Cmd/Ctrl+F)"
              aria-label="Find in diff"
              className={`px-2 py-0.5 text-[11px] font-mono rounded cursor-pointer transition-colors ${findOpen ? ACTIVE : IDLE}`}
            >
              Find
            </button>
            <div className={GROUP}>
              <ToggleButton
                pressed={settings.diffViewLayout === "unified"}
                onClick={() => update({ diffViewLayout: "unified" })}
                title="Unified diff"
              >
                Unified
              </ToggleButton>
              <ToggleButton
                pressed={split}
                onClick={() => update({ diffViewLayout: "split" })}
                title={
                  split && !isWide
                    ? "Split selected, but this pane is too narrow; showing unified"
                    : "Side-by-side diff"
                }
                skin={split ? (splitActive ? ACTIVE : "bg-brand-600/40 text-white/80") : IDLE}
              >
                Split
              </ToggleButton>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
