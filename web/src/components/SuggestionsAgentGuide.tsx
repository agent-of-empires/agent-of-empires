import { useState } from "react";
import { AGENT_SUGGEST_PROMPT } from "../lib/suggestionProviders";

/** First-run explanation while no plugin provides suggestions: any session can push them. */
export function SuggestionsAgentGuide() {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <div data-testid="suggestions-agent-guide">
      <p>
        Suggested sessions are things worth starting next. Ask any session to suggest some, and they stay here until you
        start or dismiss them.{" "}
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
          className="cursor-pointer text-brand-500 hover:underline"
        >
          {open ? "Hide prompt" : "Show a prompt to paste"}
        </button>
      </p>
      {open && (
        <div className="mt-2">
          <pre className="whitespace-pre-wrap rounded-md bg-surface-800 p-2 font-mono text-[11px] text-text-secondary">
            {AGENT_SUGGEST_PROMPT}
          </pre>
          <button
            type="button"
            onClick={() => {
              void navigator.clipboard?.writeText(AGENT_SUGGEST_PROMPT).then(() => setCopied(true));
            }}
            className="mt-1 cursor-pointer text-brand-500 hover:underline"
          >
            {copied ? "Copied" : "Copy prompt"}
          </button>
        </div>
      )}
    </div>
  );
}
