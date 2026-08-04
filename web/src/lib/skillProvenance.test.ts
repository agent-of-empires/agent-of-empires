import { describe, expect, it } from "vitest";

import type { SkillProvenance, SkillRoot, SkillsResponse } from "./api";
import { buildSkillIndex, labelForProvenance, resolveSkillSource } from "./skillProvenance";

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
      ["aoe-review", { kind: "single", label: "AoE" }],
      // Single source, matched by directory key.
      ["review-dir", { kind: "single", label: "Claude" }],
      // Single source, matched by the diverging frontmatter name key.
      ["diverge-name", { kind: "single", label: "Claude" }],
      // Ambiguous: two distinct skills/roots collide on "shared".
      ["shared", { kind: "multiple" }],
      // Same label reached via two different skills/keys is ONE source.
      ["dupkey", { kind: "single", label: "AoE" }],
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
