import { Tooltip } from "../../Tooltip";

interface Props {
  count: number;
  sendEnabled: boolean;
  /** Required, and phrased as cause + remedy: this is the only place the user
   *  ever learns why the Send button is dead. */
  sendDisabledReason: string;
  onSend: () => void;
  onDiscardAll: () => void;
}

/** Floating chip rendered above the right-panel diff list. Visible
 *  whenever the active session has at least one comment and supports
 *  the feature (acp-only). The send button is disabled only for a
 *  trashed session, which never resumes a worker to drain into. */
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
        {/* Tooltip, not the native `title`: a `disabled` button receives no
            pointer events, so the browser never shows its `title` and the
            reason the send is blocked stays invisible. The Tooltip's hover
            handlers live on the wrapper span, which is not disabled. */}
        <Tooltip text={sendEnabled ? "Send comments to agent" : sendDisabledReason} multiline>
          <button
            type="button"
            onClick={onSend}
            disabled={!sendEnabled}
            className="px-2 py-0.5 rounded bg-brand-600 text-white hover:bg-brand-500 disabled:bg-surface-700 disabled:text-text-dim disabled:cursor-not-allowed cursor-pointer transition-colors"
          >
            Send
          </button>
        </Tooltip>
      </div>
    </div>
  );
}
