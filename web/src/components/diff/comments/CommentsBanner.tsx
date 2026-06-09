interface Props {
  count: number;
  sendEnabled: boolean;
  sendDisabledReason?: string;
  onSend: () => void;
  onDiscardAll: () => void;
}

/** Floating chip rendered above the right-panel diff list. Visible
 *  whenever the active session has at least one comment and supports
 *  the feature (acp-only). The send button is disabled while the
 *  structured view worker is not running so the prompt doesn't sink. */
export function CommentsBanner({ count, sendEnabled, sendDisabledReason, onSend, onDiscardAll }: Props) {
  if (count === 0) return null;
  return (
    <div className="flex items-center gap-2 px-3 py-1.5 bg-brand-600/10 border-b border-brand-600/30 text-[11px] font-mono">
      <span className="text-brand-500 font-semibold">
        {count} comment{count === 1 ? "" : "s"}
      </span>
      <span className="text-text-dim hidden sm:inline">Cmd/Ctrl+Shift+S to send</span>
      <div className="ml-auto flex items-center gap-1.5">
        <button
          type="button"
          onClick={() => {
            if (window.confirm(`Discard all ${count} diff comment${count === 1 ? "" : "s"}? This can't be undone.`)) {
              onDiscardAll();
            }
          }}
          className="px-2 py-0.5 rounded text-text-dim hover:text-status-error hover:bg-surface-800 cursor-pointer transition-colors"
        >
          Discard all
        </button>
        <button
          type="button"
          onClick={onSend}
          disabled={!sendEnabled}
          title={sendEnabled ? "Send comments to agent" : sendDisabledReason}
          className="px-2 py-0.5 rounded bg-brand-600 text-white hover:bg-brand-500 disabled:bg-surface-700 disabled:text-text-dim disabled:cursor-not-allowed cursor-pointer transition-colors"
        >
          Send
        </button>
      </div>
    </div>
  );
}
