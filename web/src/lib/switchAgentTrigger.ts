// Cross-component trigger for the structured view "Switch agent" dialog.
//
// The dialog itself lives inside the structured view Composer (it prefills the
// composer with a handoff recap on confirm), so the only thing that
// relocates when the trigger moves out of the composer toolbar and into
// the sidebar row context menu is the open signal. The sidebar requests
// "open switch-agent for session X"; the Composer for that session flips
// its local `switchAgentOpen` state when the signal targets it.
//
// Mirrors the dispatch + pending-latch shape of `terminalFocus.ts`: the
// target Composer may not be mounted yet when the user picks the menu
// item on a session that is not currently open (selecting it navigates,
// and the structured view mounts a tick later). The caller stashes the
// intent here; the Composer consumes it on mount. When the row is already
// the open session the dispatched event is handled immediately.

export const OPEN_SWITCH_AGENT_EVENT = "aoe:open-switch-agent";

export interface OpenSwitchAgentDetail {
  sessionId: string;
}

let pendingSwitchAgent: string | null = null;

export function requestSwitchAgent(sessionId: string): void {
  if (typeof window === "undefined") return;
  pendingSwitchAgent = sessionId;
  window.dispatchEvent(
    new CustomEvent<OpenSwitchAgentDetail>(OPEN_SWITCH_AGENT_EVENT, {
      detail: { sessionId },
    }),
  );
}

// Returns true (and clears the latch) when the stashed request targets
// this session. The Composer calls it on mount so a navigation-then-open
// request lands once the target session's structured view is ready.
export function consumePendingSwitchAgent(sessionId: string): boolean {
  if (pendingSwitchAgent === sessionId) {
    pendingSwitchAgent = null;
    return true;
  }
  return false;
}
