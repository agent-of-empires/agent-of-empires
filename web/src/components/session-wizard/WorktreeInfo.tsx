import type { MouseEvent } from "react";
import { InfoPopover } from "./InfoPopover";

/** What the Worktree row turns on, with a link to its settings. */
export function WorktreeInfo({ onOpenSettings }: { onOpenSettings: () => void }) {
  const open = (e: MouseEvent) => {
    // A modified click keeps the browser's own new-tab or new-window behaviour.
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    onOpenSettings();
  };
  return (
    <InfoPopover title="What's a worktree?">
      <p className="mt-1 text-text-secondary">
        A separate copy of the repo on its own branch, so this Session&apos;s changes don&apos;t touch your main
        checkout. Moving the Session to Trash keeps it. Deleting it for good may remove it.
      </p>
      <a
        href="/settings/worktree"
        onClick={open}
        className="mt-2 inline-flex min-h-8 items-center text-brand-400 underline hover:opacity-80"
      >
        Worktree settings ›
      </a>
    </InfoPopover>
  );
}
