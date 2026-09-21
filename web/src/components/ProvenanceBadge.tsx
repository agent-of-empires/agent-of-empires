/** Small uppercase pill naming where something came from (an MCP server's provenance, a skill's source root, etc).
 *  */
export function ProvenanceBadge({ label, tone = "neutral" }: { label: string; tone?: "neutral" | "primary" }) {
  // Neutral was surface-700 on text-secondary, which measured 1.73:1 against a light theme and 4.07:1 against the
  // default dark one, so the pill was at or below the AA floor everywhere.
  const toneClass = tone === "primary" ? "bg-brand-600/15 text-brand-300" : "bg-surface-800 text-text-primary";
  return (
    // data-tone so a test can assert the distinction is being drawn without
    // pinning the exact utility classes, which are free to change.
    <span
      data-tone={tone}
      className={`font-mono text-[11px] uppercase tracking-wider px-1.5 py-0.5 rounded-full ${toneClass}`}
    >
      {label}
    </span>
  );
}
