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
    {
      directory: "aoe-review",
      name: "aoe-review",
      description: "",
      provenance: { kind: "aoe-managed" },
      provenanceLabel: "aoe-managed",
      writable: true,
    },
    {
      directory: "review-dir",
      name: "diverge-name",
      description: "",
      provenance: { kind: "external", root: "claude-user" },
      provenanceLabel: "external:claude-user",
      writable: false,
    },
    {
      directory: "orphan-dir",
      name: "orphan-dir",
      description: "",
      provenance: { kind: "external", root: "mystery-root" },
      provenanceLabel: "external:mystery-root",
      writable: false,
    },
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
      ["aoe-review", { kind: "single", label: "AoE", managed: true }],
      ["review-dir", { kind: "single", label: "Claude", managed: false }],
      ["diverge-name", { kind: "single", label: "Claude", managed: false }],
      ["shared", { kind: "multiple" }],
      ["dupkey", { kind: "single", label: "AoE", managed: true }],
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

  it("degrades to an empty index for a malformed response", () => {
    const cases: Array<[string, unknown, string | null]> = [
      ["skills missing", {}, null],
      ["skills not an array", { skills: null, roots }, null],
      ["roots missing", { skills: response.skills }, "AoE"],
      ["roots not an array", { skills: response.skills, roots: "nope" }, "AoE"],
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
      ["aoe-review", "primary"],
      ["review-dir", "neutral"],
      ["shared", "neutral"],
    ];
    for (const [name, expected] of cases) {
      const source = resolveSkillSource(index, name);
      expect(source, name).not.toBeNull();
      expect(badgeTone(source!), name).toBe(expected);
    }
  });
});
