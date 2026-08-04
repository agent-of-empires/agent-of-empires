// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import type { SkillDetail, SkillMutationResult, SkillSummary, SkillSyncResult, SkillsResponse } from "../../lib/api";

const fetchSkills = vi.fn<[], Promise<SkillsResponse | null>>();
const fetchSkill = vi.fn<[string, string], Promise<SkillDetail | null>>();
const createSkill = vi.fn<[string, string?], Promise<SkillMutationResult>>();
const updateSkill = vi.fn<[string, string], Promise<SkillMutationResult>>();
const deleteSkill = vi.fn<[string], Promise<SkillMutationResult>>();
const adoptSkill = vi.fn<[string, string, string?], Promise<SkillMutationResult>>();
const syncSkills = vi.fn<
  [{ roots?: string[]; replace?: string[]; directories?: string[] }?],
  Promise<SkillSyncResult>
>();

vi.mock("../../lib/api", () => ({
  fetchSkills: () => fetchSkills(),
  fetchSkill: (source: string, directory: string) => fetchSkill(source, directory),
  createSkill: (directory: string, description?: string) => createSkill(directory, description),
  updateSkill: (directory: string, content: string) => updateSkill(directory, content),
  deleteSkill: (directory: string) => deleteSkill(directory),
  adoptSkill: (source: string, directory: string, destination?: string) => adoptSkill(source, directory, destination),
  syncSkills: (options?: { roots?: string[]; replace?: string[]; directories?: string[] }) => syncSkills(options),
}));

import { SkillsManager } from "../SkillsManager";
import { skillBody } from "../../lib/skillBody";

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

function openCreateForm(): void {
  fireEvent.click(screen.getByText("+ New skill"));
}

beforeEach(() => {
  fetchSkills.mockReset();
  fetchSkill.mockReset();
  createSkill.mockReset();
  updateSkill.mockReset();
  deleteSkill.mockReset();
  adoptSkill.mockReset();
  syncSkills.mockReset();
  fetchSkills.mockResolvedValue(response());
  fetchSkill.mockImplementation(async (source, directory) =>
    detail(source === "aoe-managed" ? managed : { ...external, directory }),
  );
  createSkill.mockResolvedValue({ ok: true, directory: "new-skill" });
  updateSkill.mockResolvedValue({ ok: true });
  deleteSkill.mockResolvedValue({ ok: true });
  adoptSkill.mockResolvedValue({ ok: true, directory: "review" });
  syncSkills.mockResolvedValue({ ok: true, outcomes: [] });
});

describe("SkillsManager", () => {
  it("adopts an external package and hides/reveals its managed duplicate", async () => {
    fetchSkills
      .mockResolvedValueOnce(response())
      .mockResolvedValueOnce(response([{ ...external }, { ...managed, directory: "review", name: "Review" }]));
    render(<SkillsManager />);

    await screen.findByText("Available to adopt (1)");
    fireEvent.click(skillButton("review"));
    expect(await screen.findByText(/External skills are read-only/)).toBeTruthy();
    fireEvent.click(screen.getByText("Adopt into AoE"));
    await waitFor(() => expect(adoptSkill).toHaveBeenCalledWith("claude-user", "review", undefined));
    expect(await screen.findByText("Skill adopted into AoE's managed store.")).toBeTruthy();

    // The adopted copy is now managed; the original external listing is
    // hidden by the "hide already managed" default.
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
    expect(await screen.findByText("All changes saved")).toBeTruthy();

    openCreateForm();
    fireEvent.change(screen.getByLabelText("New skill directory"), { target: { value: "new-skill" } });
    fireEvent.change(screen.getByLabelText("New skill description"), { target: { value: "New instructions" } });
    fireEvent.click(screen.getByText("Create"));
    await waitFor(() => expect(createSkill).toHaveBeenCalledWith("new-skill", "New instructions"));

    fireEvent.click(screen.getByText("Delete"));
    await waitFor(() => expect(deleteSkill).toHaveBeenCalledWith("mine"));
    confirm.mockRestore();
  });

  // A sync refreshes the skill list, which used to hand the read effect a new
  // `selected` object reference and silently reset the editor to the on-disk
  // content. The selection has not changed, so the draft must survive.
  it("keeps an unsaved edit across an action that reloads the skill list", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    // Fresh skill objects per call, as parsing a real JSON response gives. The
    // bug was those new references re-firing the read effect, so the default
    // mock (one shared response, reusing the same element objects) hides it.
    fetchSkills.mockImplementation(async () => response([{ ...managed }, { ...external }]));
    render(<SkillsManager />);

    const editor = await screen.findByLabelText("SKILL.md content");
    fireEvent.change(editor, { target: { value: "unsaved work" } });
    expect(screen.getByText("Unsaved changes")).toBeTruthy();

    fireEvent.click(screen.getByText("Share with all agents"));
    await waitFor(() => expect(syncSkills).toHaveBeenCalled());
    await waitFor(() => expect(fetchSkills).toHaveBeenCalledTimes(2));

    expect((screen.getByLabelText("SKILL.md content") as HTMLTextAreaElement).value).toBe("unsaved work");
    expect(screen.getByText("Unsaved changes")).toBeTruthy();
    // Sharing keeps the same skill selected, so it must not have prompted at all.
    expect(confirm).not.toHaveBeenCalled();
    confirm.mockRestore();
  });

  // Creating jumps the selection to the new skill, which does discard the
  // draft, so unlike sharing it has to ask first.
  it("asks before a create discards an unsaved edit", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<SkillsManager />);

    const editor = await screen.findByLabelText("SKILL.md content");
    fireEvent.change(editor, { target: { value: "unsaved work" } });

    openCreateForm();
    fireEvent.change(screen.getByLabelText("New skill directory"), { target: { value: "new-skill" } });
    fireEvent.click(screen.getByText("Create"));

    expect(confirm).toHaveBeenCalledWith("Discard unsaved changes to this skill?");
    expect(createSkill).not.toHaveBeenCalled();
    expect((screen.getByLabelText("SKILL.md content") as HTMLTextAreaElement).value).toBe("unsaved work");
    confirm.mockRestore();
  });

  it("supports discarding an in-progress edit back to the loaded content", async () => {
    render(<SkillsManager />);

    const editor = (await screen.findByLabelText("SKILL.md content")) as HTMLTextAreaElement;
    const original = editor.value;
    fireEvent.change(editor, { target: { value: "scratch edit" } });
    expect(screen.getByText("Unsaved changes")).toBeTruthy();

    fireEvent.click(screen.getByText("Discard"));
    expect((screen.getByLabelText("SKILL.md content") as HTMLTextAreaElement).value).toBe(original);
    expect(screen.getByText("All changes saved")).toBeTruthy();
  });

  it("surfaces list and mutation failures", async () => {
    fetchSkills.mockResolvedValueOnce(null);
    const { unmount } = render(<SkillsManager />);
    expect(await screen.findByText("Could not load skills.")).toBeTruthy();
    unmount();

    fetchSkills.mockResolvedValue(response());
    createSkill.mockResolvedValue({ ok: false, error: "already exists", status: 409 });
    render(<SkillsManager />);
    await screen.findByLabelText("SKILL.md content");
    openCreateForm();
    fireEvent.change(screen.getByLabelText("New skill directory"), { target: { value: "mine" } });
    fireEvent.click(screen.getByText("Create"));
    expect(await screen.findByText("already exists")).toBeTruthy();
  });

  it("shares skills with all agents, showing conflicts but not unchanged outcomes", async () => {
    syncSkills.mockResolvedValue({
      ok: true,
      outcomes: [
        { root: "claude-user", directory: "review", status: "created", message: null },
        { root: "codex-user", directory: "review", status: "unchanged", message: null },
        { root: "claude-user", directory: "mine", status: "conflict", message: "user edited the propagated copy" },
      ],
    });
    render(<SkillsManager />);

    fireEvent.click(await screen.findByText("Share with all agents"));
    await waitFor(() => expect(syncSkills).toHaveBeenCalledWith(undefined));
    expect(await screen.findByText("1 shared, 1 unchanged, 1 conflict")).toBeTruthy();
    expect(screen.getByText(/conflict claude-user\/mine: user edited the propagated copy/)).toBeTruthy();
    expect(screen.queryByText(/codex-user\/review/)).toBeNull();
    // load() re-runs after a successful sync.
    await waitFor(() => expect(fetchSkills).toHaveBeenCalledTimes(2));
  });

  it("surfaces a failed sync request through the existing error notice", async () => {
    syncSkills.mockResolvedValue({ ok: false, outcomes: [], error: "read only" });
    render(<SkillsManager />);

    fireEvent.click(await screen.findByText("Share with all agents"));
    expect(await screen.findByText("read only")).toBeTruthy();
  });

  it("replaces a conflicting row on request, re-issuing sync scoped to that root and directory", async () => {
    syncSkills
      .mockResolvedValueOnce({
        ok: true,
        outcomes: [
          { root: "claude-user", directory: "mine", status: "conflict", message: "user edited the propagated copy" },
        ],
      })
      .mockResolvedValueOnce({
        ok: true,
        outcomes: [{ root: "claude-user", directory: "mine", status: "updated", message: "replaced on request" }],
      });
    render(<SkillsManager />);

    fireEvent.click(await screen.findByText("Share with all agents"));
    const replaceButton = await screen.findByLabelText("Replace mine in claude-user");
    fireEvent.click(replaceButton);

    await waitFor(() => expect(syncSkills).toHaveBeenLastCalledWith({ roots: ["claude-user"], replace: ["mine"] }));
    // The row that was replaced no longer reports a conflict, so its Replace
    // button and the conflict/error list disappear; the summary reflects it.
    expect(await screen.findByText("1 shared")).toBeTruthy();
    expect(screen.queryByLabelText("Replace mine in claude-user")).toBeNull();
  });

  it("shares only the selected skill, scoping the sync request to its directory", async () => {
    syncSkills.mockResolvedValue({
      ok: true,
      outcomes: [{ root: "claude-user", directory: "mine", status: "updated", message: null }],
    });
    render(<SkillsManager />);

    // "mine" (the managed skill) is selected by default.
    await screen.findByLabelText("SKILL.md content");
    fireEvent.click(screen.getByText("Share this skill"));

    await waitFor(() => expect(syncSkills).toHaveBeenCalledWith({ directories: ["mine"] }));
    expect(await screen.findByText("1 shared")).toBeTruthy();

    // Sharing an unsaved edit would ship stale content, so the button
    // disables itself while the draft is dirty.
    fireEvent.change(screen.getByLabelText("SKILL.md content"), { target: { value: "changed" } });
    expect((screen.getByText("Share this skill").closest("button") as HTMLButtonElement).disabled).toBe(true);
  });

  it("switches to Preview and renders the draft as markdown instead of the raw textarea", async () => {
    render(<SkillsManager />);

    await screen.findByLabelText("SKILL.md content");
    fireEvent.click(screen.getByText("Preview"));

    expect(screen.queryByLabelText("SKILL.md content")).toBeNull();
    expect(await screen.findByText(/body/)).toBeTruthy();

    fireEvent.click(screen.getByText("Raw"));
    expect(await screen.findByLabelText("SKILL.md content")).toBeTruthy();
  });

  it("disables mutating controls in readOnly mode but leaves selection and Raw/Preview working", async () => {
    render(<SkillsManager readOnly />);

    await screen.findByLabelText("SKILL.md content");
    expect(screen.getByText(/read-only, so skills cannot be/)).toBeTruthy();
    expect((screen.getByText("+ New skill").closest("button") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByText("Share with all agents").closest("button") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByText("Delete").closest("button") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByText("Share this skill").closest("button") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByLabelText("SKILL.md content") as HTMLTextAreaElement).readOnly).toBe(true);

    // Selection still works, and switching to an external (non-writable)
    // skill shows a disabled "Adopt into AoE" instead of Save/Delete.
    fireEvent.click(skillButton("review"));
    expect(await screen.findByText("Adopt into AoE")).toBeTruthy();
    expect((screen.getByText("Adopt into AoE").closest("button") as HTMLButtonElement).disabled).toBe(true);

    // Raw/Preview toggling still works.
    fireEvent.click(screen.getByText("Preview"));
    expect(screen.queryByLabelText("SKILL.md content")).toBeNull();
    fireEvent.click(screen.getByText("Raw"));
    expect(await screen.findByLabelText("SKILL.md content")).toBeTruthy();
  });

  it("collapses a sidebar group to hide its rows without discarding the group", async () => {
    render(<SkillsManager />);

    await screen.findByText("Managed (1)");
    expect(skillButton("mine")).toBeTruthy();

    fireEvent.click(screen.getByText("Managed (1)"));
    expect(screen.queryByText("mine", { selector: "button span" })).toBeNull();

    fireEvent.click(screen.getByText("Managed (1)"));
    expect(skillButton("mine")).toBeTruthy();
  });
});

describe("skillBody", () => {
  it("strips the frontmatter fence so the preview shows instructions, not YAML", () => {
    const cases: Array<[string, string, string]> = [
      ["plain fence", "---\nname: a\ndescription: b\n---\n# Title\n", "# Title\n"],
      ["crlf fence", "---\r\nname: a\r\ndescription: b\r\n---\r\n# Title\n", "# Title\n"],
      ["leading BOM", "\ufeff---\nname: a\ndescription: b\n---\nbody\n", "body\n"],
      // No closing fence is malformed, but a preview that renders nothing is
      // worse than one that renders the raw text.
      ["unterminated fence", "---\nname: a\nbody\n", "---\nname: a\nbody\n"],
      ["no frontmatter", "# Just a heading\n", "# Just a heading\n"],
      // `----` is not a closing fence; the file is malformed, so it is
      // preserved rather than losing its first hyphen.
      ["over-long fence", "---\nname: a\n----\nbody\n", "---\nname: a\n----\nbody\n"],
      ["fence followed by text", "---\nname: a\n---text\n", "---\nname: a\n---text\n"],
      // A rule inside the body must not be mistaken for a fence close.
      ["hr in body", "---\nname: a\n---\nintro\n\n---\n\nmore\n", "intro\n\n---\n\nmore\n"],
    ];
    for (const [label, input, expected] of cases) {
      expect(skillBody(input), label).toBe(expected);
    }
  });
});

describe("SkillsManager provenance tint", () => {
  it("brands AoE-managed rows and leaves external ones neutral", async () => {
    render(<SkillsManager />);
    await waitFor(() => expect(skillButton("mine")).toBeTruthy());

    const toneFor = (directory: string) =>
      skillButton(directory)?.querySelector("[data-tone]")?.getAttribute("data-tone");

    // The whole point of the tint: AoE's own skills are pickable out of a
    // mixed list without reading the label.
    expect(toneFor("mine")).toBe("primary");
    expect(toneFor("review")).toBe("neutral");
  });
});
