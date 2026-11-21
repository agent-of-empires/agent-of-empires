import { InfoPopover } from "./InfoPopover";

/** What the Worktree row turns on. The link opens a new tab so the wizard keeps its form. */
export function WorktreeInfo() {
  return (
    <InfoPopover title="What's a worktree?">
      <p className="mt-1 text-text-secondary">
        A separate copy of the repo on its own branch, so this Session&apos;s changes don&apos;t touch your main
        checkout. AoE removes it when you delete the Session, unless you choose to keep it.
      </p>
      <a
        href="/settings/worktree"
        target="_blank"
        rel="noopener"
        className="mt-2 inline-flex min-h-8 items-center text-brand-400 underline hover:opacity-80"
      >
        Worktree settings ›
      </a>
    </InfoPopover>
  );
}
