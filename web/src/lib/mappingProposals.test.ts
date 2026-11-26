import { describe, expect, it } from "vitest";
import { proposeMappings } from "./mappingProposals";
import type { SuggestionItem, SuggestionProject } from "./types";

const item = (mapping_key: string | null, provider_id = "gh"): SuggestionItem => ({
  id: `${provider_id}-${mapping_key}`,
  provider_id,
  title: "",
  reason: "",
  source_url: "",
  source_ref: "",
  preview: "",
  criterion: "assigned",
  mapping_key,
  draft: { title: "", prompt: "" },
  dismissed: false,
});

const projects: SuggestionProject[] = [
  { name: "Website", path: "/work/site", origin: "Acme/Web" },
  { name: "web", path: "/work/web", origin: null },
  { name: "billing", path: "/work/billing_service", origin: "someone-else/billing" },
  { name: "tooling", path: "/work/my_tool", origin: null },
];

describe("proposeMappings", () => {
  it("prefers an origin match, then a loose name or folder match, else nothing", () => {
    const got = proposeMappings(
      [item("acme/web"), item("acme/web"), item("acme/billing-service"), item("acme/my-tool"), item("acme/none")],
      [],
      projects,
    );
    expect(got.map(({ mapping_key, count, project, match }) => [mapping_key, count, project, match])).toEqual([
      ["acme/billing-service", 1, "/work/billing_service", "name"],
      ["acme/my-tool", 1, "/work/my_tool", "name"],
      ["acme/none", 1, null, null],
      ["acme/web", 2, "/work/site", "origin"],
    ]);
  });

  it("matches a slashless key by name and skips mapped and keyless items", () => {
    const got = proposeMappings(
      [item("web"), item("acme/web"), item("acme/web", "other"), item(null)],
      [{ provider_id: "gh", mapping_key: "acme/web", projects: ["/work/site"] }],
      projects,
    );
    expect(got.map((p) => [p.provider_id, p.mapping_key, p.project, p.match])).toEqual([
      ["other", "acme/web", "/work/site", "origin"],
      ["gh", "web", "/work/web", "name"],
    ]);
  });
});
