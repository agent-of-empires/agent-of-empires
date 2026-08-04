/** Small uppercase pill naming where something came from (an MCP server's
 *  provenance, a skill's source root, etc). Shared across the skills manager,
 *  the `/` slash-command picker, and the skill tool-call card so provenance
 *  reads identically everywhere it appears (#3052). */
export function ProvenanceBadge({ label }: { label: string }) {
  return (
    <span className="font-mono text-[11px] uppercase tracking-wider px-1.5 py-0.5 rounded bg-surface-700 text-text-secondary">
      {label}
    </span>
  );
}
