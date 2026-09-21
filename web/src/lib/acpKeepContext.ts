// Confirm-dialog copy for switching views, matching the TUI wording.

export interface SwitchViewCopy {
  title: string;
  body: string;
  confirmLabel: string;
}

/** `keepsContext` comes from the server's `keeps_context`. */
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
