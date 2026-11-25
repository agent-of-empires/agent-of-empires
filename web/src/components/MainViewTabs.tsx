import { FileText, Sparkles, X } from "lucide-react";

interface Props {
  /** The open file's repo-relative or absolute path. */
  filePath: string;
  /** The file, not the agent, fills the main area. */
  fileShown: boolean;
  onShowAgent: () => void;
  onShowFile: () => void;
  onCloseFile: () => void;
}

const tab = (active: boolean) =>
  `group flex items-center gap-1 pl-2 pr-1 border-r border-surface-700/20 transition-colors ${
    active ? "bg-surface-800 text-text-secondary" : "text-text-dim hover:text-text-secondary hover:bg-surface-800/40"
  }`;

/** The main area's tabs while a file is open: the agent and that one file, which the next pick replaces. */
export function MainViewTabs({ filePath, fileShown, onShowAgent, onShowFile, onCloseFile }: Props) {
  const name = filePath.split("/").pop() || filePath;
  return (
    <div
      role="tablist"
      aria-label="Main view"
      data-testid="main-view-tabs"
      className="flex items-stretch h-7 shrink-0 bg-surface-900 border-b border-surface-700/20"
    >
      <div className={tab(!fileShown)}>
        <button
          type="button"
          role="tab"
          aria-selected={!fileShown}
          data-testid="main-view-tab-agent"
          onClick={onShowAgent}
          className="flex items-center gap-1 pr-1 cursor-pointer"
        >
          <Sparkles className="size-3.5 shrink-0" aria-hidden />
          <span className="text-[11px] font-medium">Agent</span>
        </button>
      </div>
      <div className={`${tab(fileShown)} min-w-0`}>
        <button
          type="button"
          role="tab"
          aria-selected={fileShown}
          title={filePath}
          data-testid="main-view-tab-file"
          onClick={onShowFile}
          className="flex items-center gap-1 min-w-0 cursor-pointer"
        >
          <FileText className="size-3.5 shrink-0" aria-hidden />
          <span className="text-[11px] font-mono truncate max-w-[16rem]">{name}</span>
        </button>
        <button
          type="button"
          onClick={onCloseFile}
          title={`Close ${name}`}
          aria-label={`Close ${name}`}
          className="w-5 h-5 flex items-center justify-center shrink-0 rounded text-text-dim hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
        >
          <X className="size-3" aria-hidden />
        </button>
      </div>
    </div>
  );
}
