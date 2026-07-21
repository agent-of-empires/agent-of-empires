// Client mirror of the backend view-switch context-preservation gate
// (`agents::acp_transcript_cli_resumable` in src/agents.rs) plus the
// capability-aware confirm copy the TUI shows (src/tui/home/input.rs
// `prompt_switch_view_for_selected`). The server has no "keeps_context" flag on
// the session shape, so the dashboard computes it from `tool` + `acp_agent`.

/** Whether switching this pairing preserves the conversation across a view
 *  swap. Only claude qualifies: claude-agent-acp and the terminal `claude` CLI
 *  share one CLI-resumable transcript. `acpAgent` is the active adapter
 *  (session `acp_agent`, which falls back to `tool` when unset). */
export function acpTranscriptCliResumable(tool: string, acpAgent: string): boolean {
  return tool === "claude" && (acpAgent === "claude" || acpAgent === "claude-code");
}

export interface SwitchViewCopy {
  title: string;
  body: string;
  confirmLabel: string;
}

/** Title + body + confirm label for the switch-view confirm dialog, matching
 *  the TUI wording. `toStructured` is the switch direction; `keepsContext` is
 *  `acpTranscriptCliResumable(tool, acpAgent)`. Claude keeps the conversation
 *  in both directions; other agents restart fresh. */
export function switchViewCopy(toStructured: boolean, keepsContext: boolean): SwitchViewCopy {
  if (toStructured) {
    return {
      title: "Switch to structured view",
      confirmLabel: "Switch to structured",
      body: keepsContext
        ? "Switch this session to the structured view? The tmux pane and its scrollback are cleared, but the conversation continues in structured view; the agent restarts under the aoe serve daemon."
        : "Switch this session to the structured view? The tmux pane and its scrollback are destroyed; the agent restarts under the aoe serve daemon with a fresh conversation.",
    };
  }
  return {
    title: "Switch to terminal",
    confirmLabel: "Switch to terminal",
    body: keepsContext
      ? "Switch this session back to a tmux terminal? The conversation continues in the terminal (the agent resumes it with `--resume`); the structured view is closed."
      : "Switch this session back to a tmux terminal? The structured conversation is closed; the agent restarts in a fresh terminal pane.",
  };
}
