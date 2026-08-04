// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import type { SkillDetail, SkillMutationResult, SkillSummary, SkillsResponse } from "../../lib/api";

const fetchSkills = vi.fn<[], Promise<SkillsResponse | null>>();
const fetchSkill = vi.fn<[string, string], Promise<SkillDetail | null>>();
const createSkill = vi.fn<[string, string?], Promise<SkillMutationResult>>();
const updateSkill = vi.fn<[string, string], Promise<SkillMutationResult>>();
const deleteSkill = vi.fn<[string], Promise<SkillMutationResult>>();
const adoptSkill = vi.fn<[string, string, string?], Promise<SkillMutationResult>>();

vi.mock("../../lib/api", () => ({
  fetchSkills: () => fetchSkills(),
  fetchSkill: (source: string, directory: string) => fetchSkill(source, directory),
  createSkill: (directory: string, description?: string) => createSkill(directory, description),
  updateSkill: (directory: string, content: string) => updateSkill(directory, content),
  deleteSkill: (directory: string) => deleteSkill(directory),
  adoptSkill: (source: string, directory: string, destination?: string) => adoptSkill(source, directory, destination),
}));

import { SkillsManager } from "../SkillsManager";

const managed: SkillSummary = {
  directory: "mine",
  name: "Mine",
  description: "Managed instructions",
  provenance: { kind: "aoe-managed" },
  provenanceLabel: "aoe-managed",
  writable: true,
};

const external: SkillSummary = {
  directory: "review",
  name: "Review",
  description: "Review code carefully",
  provenance: { kind: "external", root: "claude-user" },
  provenanceLabel: "external:claude-user",
  writable: false,
};

const response = (skills: SkillSummary[] = [managed, external]): SkillsResponse => ({
  skills,
  roots: [
    {
      id: "claude-user",
      label: "Claude",
      relativePath: ".claude/skills",
      consumers: ["claude"],
      legacy: false,
    },
  ],
});

function detail(skill: SkillSummary): SkillDetail {
  return {
    directory: skill.directory,
    name: skill.name,
    description: skill.description,
    provenance: skill.provenance,
    content: `---\nname: ${skill.directory}\ndescription: ${skill.description}\n---\n\nbody\n`,
  };
}

function skillButton(directory: string): HTMLButtonElement {
  const label = screen.getByText(directory, { selector: "button span" });
  const button = label.closest("button");
  if (!button) throw new Error(`missing skill button for ${directory}`);
  return button;
}

beforeEach(() => {
  fetchSkills.mockReset();
  fetchSkill.mockReset();
  createSkill.mockReset();
  updateSkill.mockReset();
  deleteSkill.mockReset();
  adoptSkill.mockReset();
  fetchSkills.mockResolvedValue(response());
  fetchSkill.mockImplementation(async (source, directory) =>
    detail(source === "aoe-managed" ? managed : { ...external, directory }),
  );
  createSkill.mockResolvedValue({ ok: true, directory: "new-skill" });
  updateSkill.mockResolvedValue({ ok: true });
  deleteSkill.mockResolvedValue({ ok: true });
  adoptSkill.mockResolvedValue({ ok: true, directory: "review" });
});

describe("SkillsManager", () => {
  it("filters source-qualified skills and adopts an external package", async () => {
    fetchSkills
      .mockResolvedValueOnce(response())
      .mockResolvedValueOnce(response([{ ...external }, { ...managed, directory: "review", name: "Review" }]));
    render(<SkillsManager />);

    fireEvent.change(await screen.findByLabelText("Filter skills by source"), {
      target: { value: "claude-user" },
    });
    expect(skillButton("review")).toBeTruthy();
    expect(screen.queryByText("mine", { selector: "button span" })).toBeNull();

    fireEvent.click(skillButton("review"));
    expect(await screen.findByText(/External skills are read-only/)).toBeTruthy();
    fireEvent.click(screen.getByText("Adopt into AoE"));
    await waitFor(() => expect(adoptSkill).toHaveBeenCalledWith("claude-user", "review", undefined));
    expect(await screen.findByText("Skill adopted into AoE's managed store.")).toBeTruthy();

    expect(screen.getAllByText("review", { selector: "button span" })).toHaveLength(1);
    fireEvent.click(screen.getByLabelText("Hide external skills already managed"));
    expect(screen.getAllByText("review", { selector: "button span" })).toHaveLength(2);
  });

  it("creates, edits, and deletes managed skills with dirty-state protection", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);
    render(<SkillsManager />);

    const editor = await screen.findByLabelText("SKILL.md content");
    fireEvent.change(editor, { target: { value: "changed content" } });
    expect(screen.getByText("Unsaved changes")).toBeTruthy();

    fireEvent.click(skillButton("review"));
    expect(confirm).toHaveBeenCalledWith("Discard unsaved changes to this skill?");
    expect((screen.getByLabelText("SKILL.md content") as HTMLTextAreaElement).value).toBe("changed content");

    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(updateSkill).toHaveBeenCalledWith("mine", "changed content"));

    fireEvent.change(screen.getByLabelText("New skill directory"), { target: { value: "new-skill" } });
    fireEvent.change(screen.getByLabelText("New skill description"), { target: { value: "New instructions" } });
    fireEvent.click(screen.getByText("Create"));
    await waitFor(() => expect(createSkill).toHaveBeenCalledWith("new-skill", "New instructions"));

    fireEvent.click(screen.getByText("Delete"));
    await waitFor(() => expect(deleteSkill).toHaveBeenCalledWith("mine"));
    confirm.mockRestore();
  });

  it("surfaces list and mutation failures", async () => {
    fetchSkills.mockResolvedValueOnce(null);
    const { unmount } = render(<SkillsManager />);
    expect(await screen.findByText("Could not load skills.")).toBeTruthy();
    unmount();

    fetchSkills.mockResolvedValue(response());
    createSkill.mockResolvedValue({ ok: false, error: "already exists", status: 409 });
    render(<SkillsManager />);
    fireEvent.change(await screen.findByLabelText("New skill directory"), { target: { value: "mine" } });
    fireEvent.click(screen.getByText("Create"));
    expect(await screen.findByText("already exists")).toBeTruthy();
  });
});
