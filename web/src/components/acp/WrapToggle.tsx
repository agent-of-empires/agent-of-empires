/* eslint-disable react-refresh/only-export-components */
// Per-block line-wrap toggle for tool output and code blocks. Blocks start from `acp.wrap_tool_output`.

import { useCallback, useState } from "react";
import { WrapText } from "lucide-react";

import { useAcpPrefs } from "../../lib/acpPrefs";

export function useWrapState(): [boolean, () => void] {
  const { wrapToolOutput } = useAcpPrefs();
  // Null until the user flips this block, so an untouched block follows the setting.
  const [override, setOverride] = useState<boolean | null>(null);
  const wrapped = override ?? wrapToolOutput;
  const toggle = useCallback(() => setOverride(!wrapped), [wrapped]);
  return [wrapped, toggle];
}

export function WrapToggle({ wrapped, onToggle }: { wrapped: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      aria-pressed={wrapped}
      title={wrapped ? "Line wrap on; click to scroll instead" : "Wrap long lines"}
      onClick={(e) => {
        e.stopPropagation();
        onToggle();
      }}
      className={[
        "inline-flex min-h-8 items-center gap-1 rounded px-2 text-[10px] uppercase tracking-wider",
        wrapped ? "bg-brand-700/10 text-brand-400" : "text-text-dim hover:bg-surface-800 hover:text-text-secondary",
      ].join(" ")}
    >
      <WrapText className="h-3 w-3" />
      Wrap
    </button>
  );
}

/** Slim in-flow strip for blocks without a header of their own. */
export function WrapBar({ wrapped, onToggle }: { wrapped: boolean; onToggle: () => void }) {
  return (
    <div className="flex justify-end px-1">
      <WrapToggle wrapped={wrapped} onToggle={onToggle} />
    </div>
  );
}

/** One element per source line, so CSS can hang-indent and mark wrapped rows. */
export function WrapLines({ text }: { text: string }) {
  return text.split("\n").map((line, i) => (
    <span key={i} className="wrap-line">
      {line}
    </span>
  ));
}
