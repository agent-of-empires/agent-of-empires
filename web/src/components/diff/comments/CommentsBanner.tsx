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
        {/* Tooltip, not the native `title`: the browser never renders a
            `title` on a button it won't send pointer events to, so the reason
            the send is blocked stayed invisible. `aria-disabled` rather than
            `disabled` for the same reason one rung up: a natively disabled
            button is not focusable, so a keyboard user could never reach the
            explanation at all. The button stays in the tab order, announces
            itself as disabled, and `onSend` guards the click. */}
        <Tooltip text={sendEnabled ? "Send comments to agent" : sendDisabledReason} multiline>
          <button
            type="button"
            onClick={() => {
              if (sendEnabled) onSend();
            }}
            aria-disabled={!sendEnabled}
            className={`px-2 py-0.5 rounded-md transition-colors ${
              sendEnabled
                ? "bg-brand-600 text-white hover:bg-brand-500 cursor-pointer"
                : "bg-surface-700 text-text-dim cursor-not-allowed"
            }`}
          >
            Send
          </button>
        </Tooltip>
      </div>
    </div>
  );
}
