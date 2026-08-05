import { describe, expect, it } from "vitest";

import type { SkillProvenance, SkillRoot, SkillsResponse } from "./api";
import { badgeTone, buildSkillIndex, labelForProvenance, resolveSkillSource } from "./skillProvenance";

const roots: SkillRoot[] = [
  { id: "claude-user", label: "Claude", relativePath: ".claude/skills", consumers: ["claude"], legacy: false },
  { id: "gemini-user", label: "Gemini", relativePath: ".gemini/skills", consumers: ["gemini"], legacy: false },
];

const response: SkillsResponse = {
  roots,
  skills: [
    // Directory and frontmatter name match.
    {
      directory: "aoe-review",
      name: "aoe-review",
      description: "",
      provenance: { kind: "aoe-managed" },
      provenanceLabel: "aoe-managed",
      writable: true,
    },
    // Frontmatter name diverges from directory (AoE deliberately allows this).
    {
      directory: "review-dir",
      name: "diverge-name",
      description: "",
      provenance: { kind: "external", root: "claude-user" },
      provenanceLabel: "external:claude-user",
      writable: false,
    },
    // External root id absent from `roots`.
    {
      directory: "orphan-dir",
      name: "orphan-dir",
      description: "",
      provenance: { kind: "external", root: "mystery-root" },
      provenanceLabel: "external:mystery-root",
      writable: false,
    },
    // Two distinct skills whose keys collide under "shared", from different
    // roots: an agent name lookup for "shared" must read as ambiguous.
    {
      directory: "shared",
      name: "shared-a",
      description: "",
      provenance: { kind: "external", root: "claude-user" },
      provenanceLabel: "external:claude-user",
      writable: false,
    },
    {
      directory: "shared-b",
      name: "shared",
      description: "",
      provenance: { kind: "external", root: "gemini-user" },
      provenanceLabel: "external:gemini-user",
      writable: false,
    },
    // Two distinct skills whose keys collide under "dupkey" but share the
    // same label: must NOT read as ambiguous (one source, reached twice).
    {
      directory: "dupkey",
      name: "dupkey-full",
      description: "",
      provenance: { kind: "aoe-managed" },
      provenanceLabel: "aoe-managed",
      writable: true,
    },
    {
      directory: "dupkey-alt",
      name: "dupkey",
      description: "",
      provenance: { kind: "aoe-managed" },
      provenanceLabel: "aoe-managed",
      writable: true,
    },
  ],
};

const index = buildSkillIndex(response);

describe("resolveSkillSource", () => {
  it("resolves a command name to its provenance across single/ambiguous/unknown cases", () => {
    const cases: [string, ReturnType<typeof resolveSkillSource>][] = [
      // Single source, matched by directory (directory === name here).
      ["aoe-review", { kind: "single", label: "AoE", managed: true }],
      // Single source, matched by directory key.
      ["review-dir", { kind: "single", label: "Claude", managed: false }],
      // Single source, matched by the diverging frontmatter name key.
      ["diverge-name", { kind: "single", label: "Claude", managed: false }],
      // Ambiguous: two distinct skills/roots collide on "shared".
      ["shared", { kind: "multiple" }],
      // Same label reached via two different skills/keys is ONE source.
      ["dupkey", { kind: "single", label: "AoE", managed: true }],
      // Unknown command name: no badge.
      ["does-not-exist", null],
    ];
    for (const [name, expected] of cases) {
      expect(resolveSkillSource(index, name), name).toEqual(expected);
    }
  });

  it("resolves everything to null against the empty index for a null response", () => {
    const empty = buildSkillIndex(null);
    expect(resolveSkillSource(empty, "aoe-review")).toBeNull();
  });

  // `fetchJson` casts any 200 body to SkillsResponse without validating it, so
  // a surprising payload reaches buildSkillIndex as-is. It must degrade to an
  // empty index: these badges are cosmetic and must not throw into the surface
  // rendering them.
  it("degrades to an empty index for a malformed response", () => {
    // The label "aoe-review" resolves to once the response has degraded, or
    // null when nothing could be indexed at all.
    const cases: Array<[string, unknown, string | null]> = [
      ["skills missing", {}, null],
      ["skills not an array", { skills: null, roots }, null],
      // A usable skills array still indexes; only the roots lookup degrades,
      // falling labels back to the raw root id (unused by an aoe-managed one).
      ["roots missing", { skills: response.skills }, "AoE"],
      ["roots not an array", { skills: response.skills, roots: "nope" }, "AoE"],
      // A single bad member is skipped, not fatal to its neighbours.
      ["null member", { skills: [null, ...response.skills], roots }, "AoE"],
      ["member without provenance", { skills: [{ directory: "x", name: "x" }, ...response.skills], roots }, "AoE"],
    ];
    for (const [label, body, expected] of cases) {
      const built = buildSkillIndex(body as SkillsResponse);
      expect(resolveSkillSource(built, "aoe-review")?.label ?? null, label).toEqual(expected);
    }
  });
});

describe("labelForProvenance", () => {
  it("maps aoe-managed to 'AoE', a known root to its label, and an unknown root to the raw id", () => {
    const cases: [SkillProvenance, string][] = [
      [{ kind: "aoe-managed" }, "AoE"],
      [{ kind: "external", root: "claude-user" }, "Claude"],
      [{ kind: "external", root: "mystery-root" }, "mystery-root"],
    ];
    for (const [provenance, expected] of cases) {
      expect(labelForProvenance(provenance, roots), JSON.stringify(provenance)).toBe(expected);
    }
  });
});

describe("badgeTone", () => {
  it("brands only an unambiguously AoE-managed source", () => {
    const cases: Array<[string, "neutral" | "primary"]> = [
      // AoE's own store is the thing the tint is for.
      ["aoe-review", "primary"],
      // A host root is not ours, so it stays neutral and the branded one pops.
      ["review-dir", "neutral"],
      // Ambiguous: we cannot say the AoE copy is what the agent will load, so
      // claiming it in colour would be a guess presented as a fact.
      ["shared", "neutral"],
    ];
    for (const [name, expected] of cases) {
      const source = resolveSkillSource(index, name);
      expect(source, name).not.toBeNull();
      expect(badgeTone(source!), name).toBe(expected);
    }
  });
});
