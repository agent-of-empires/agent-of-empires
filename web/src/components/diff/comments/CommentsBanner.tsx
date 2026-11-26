import { BUTTON_DANGER_GHOST_SM, BUTTON_PRIMARY_SM } from "../../../lib/surfaces";
import { Tooltip } from "../../Tooltip";

// aria-disabled rather than disabled, so the BUTTON set's disabled: styles never apply.
const SEND_BLOCKED = BUTTON_PRIMARY_SM.replace("cursor-pointer", "cursor-not-allowed opacity-40").replace(
  " hover:bg-brand-500",
  "",
);

interface Props {
  count: number;
  sendEnabled: boolean;
  /** Cause plus remedy: the only place the user learns why Send is disabled. */
  sendDisabledReason: string;
  onSend: () => void;
  onDiscardAll: () => void;
}

/** Floating chip above the diff list; Send is disabled when the session cannot accept input. */
export function CommentsBanner({ count, sendEnabled, sendDisabledReason, onSend, onDiscardAll }: Props) {
  if (count === 0) return null;
  return (
    <div className="flex items-center gap-2 px-3 py-1.5 border-b border-surface-700/50 bg-surface-850 text-[12px]">
      <span className="font-medium text-text-primary">
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
          className={BUTTON_DANGER_GHOST_SM}
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
            className={sendEnabled ? BUTTON_PRIMARY_SM : SEND_BLOCKED}
          >
            Send
          </button>
        </Tooltip>
      </div>
    </div>
  );
}
