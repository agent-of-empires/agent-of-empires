import { Check, Copy as CopyIcon, type LucideIcon } from "lucide-react";

import { COMPACT_COPY_BUTTON_CLASS, COPY_BUTTON_CLASS, useCopy } from "./useCopy";

export function CopyIconState({ copied, Icon = CopyIcon }: { copied: boolean; Icon?: LucideIcon }) {
  return copied ? <Check className="h-3 w-3" /> : <Icon className="h-3 w-3" />;
}

/** Icon button that copies `source` and confirms. `className` adds position and reveal behavior. */
export function CopyMarkdownButton({
  source,
  label,
  className = "",
  compact = false,
}: {
  source: string;
  label: string;
  className?: string;
  compact?: boolean;
}) {
  const [copied, copy] = useCopy();
  const shown = copied ? "Copied" : label;
  return (
    <button
      type="button"
      aria-label={shown}
      title={shown}
      onClick={(e) => {
        e.stopPropagation();
        void copy(source);
      }}
      className={`${compact ? COMPACT_COPY_BUTTON_CLASS : COPY_BUTTON_CLASS} ${className}`}
      data-copied={copied}
    >
      <CopyIconState copied={copied} />
    </button>
  );
}
