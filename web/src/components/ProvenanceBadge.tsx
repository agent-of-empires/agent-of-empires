/** Small uppercase pill naming where something came from (an MCP server's
 *  provenance, a skill's source root, etc). Shared across the skills manager,
 *  the `/` slash-command picker, and the skill tool-call card so provenance
 *  reads identically everywhere it appears (#3052).
 *
 *  `tone` is how a surface says "this one is ours": AoE-managed skills carry
 *  the brand tint so they are pickable out of a list at a glance, while every
 *  other source stays neutral so the branded one is the thing that stands out.
 *  Neutral is the default, so the MCP panel opts into nothing; extracting this
 *  component did still change how its badge looks, picking up the contrast fix
 *  and the full radius documented below.
 */
export function ProvenanceBadge({ label, tone = "neutral" }: { label: string; tone?: "neutral" | "primary" }) {
  // Neutral was surface-700 on text-secondary, which measured 1.73:1 against a
  // light theme and 4.07:1 against the default dark one, so the pill was at or
  // below the AA floor everywhere. surface-800 on text-primary measures 6.6:1
  // or better across the light, default, and rose-pine palettes.
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
