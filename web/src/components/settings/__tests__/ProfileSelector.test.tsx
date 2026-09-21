// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProfileSelector } from "../ProfileSelector";

vi.mock("../../../lib/api", () => ({
  fetchProfiles: vi.fn(),
  createProfile: vi.fn(),
  renameProfile: vi.fn(),
  deleteProfile: vi.fn(),
}));

import { createProfile, deleteProfile, fetchProfiles, renameProfile } from "../../../lib/api";

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(fetchProfiles).mockResolvedValue([
    { name: "default", is_default: true },
    { name: "work", is_default: false },
  ]);
  vi.mocked(createProfile).mockResolvedValue(true);
  vi.mocked(renameProfile).mockResolvedValue(true);
  vi.mocked(deleteProfile).mockResolvedValue(true);
});

afterEach(cleanup);

async function openPanel(button: "+ New" | "Rename", selected = "work") {
  const onSelect = vi.fn();
  const { container } = render(<ProfileSelector selectedProfile={selected} onSelect={onSelect} />);
  await waitFor(() => expect(fetchProfiles).toHaveBeenCalled());
  fireEvent.click(screen.getByText(button));
  const input = (await screen.findByPlaceholderText(
    button === "+ New" ? "Profile name" : "New name",
  )) as HTMLInputElement;
  const submit = (value?: string) => {
    if (value !== undefined) fireEvent.change(input, { target: { value } });
    fireEvent.keyDown(input, { key: "Enter", code: "Enter" });
  };
  return { container, input, submit, onSelect };
}

const INVALID = "Only letters, digits, hyphens, and underscores";

describe("ProfileSelector create", () => {
  it.each([
    ["", "Name is required"],
    ["   ", "Name is required"],
    ...["bad name", "bad;name", "bad$name", "bad|name", "bad&name", "bad`name", "bad/name", "..", ".hidden"].map(
      (n) => [n, INVALID],
    ),
  ])("rejects %j without calling createProfile", async (name, message) => {
    const { container, submit } = await openPanel("+ New");
    submit(name);
    expect(container.textContent).toContain(message);
    expect(createProfile).not.toHaveBeenCalled();
  });

  it.each(["work", "work-2", "work_2", "A", "my-profile_42"])("creates trimmed %s", async (good) => {
    const { submit } = await openPanel("+ New");
    submit(`  ${good}  `);
    await waitFor(() => expect(createProfile).toHaveBeenCalledWith(good));
  });

  it("surfaces a create failure", async () => {
    vi.mocked(createProfile).mockResolvedValueOnce(false);
    const { container, submit } = await openPanel("+ New");
    submit("duplicate");
    await waitFor(() => expect(container.textContent).toContain("Failed to create profile"));
  });
});

describe("ProfileSelector rename", () => {
  it("closes without a request when the name is unchanged", async () => {
    const { input, submit } = await openPanel("Rename");
    expect(input.value).toBe("work");
    submit();
    expect(renameProfile).not.toHaveBeenCalled();
    expect(screen.queryByPlaceholderText("New name")).toBeNull();
  });

  it("rejects an invalid name", async () => {
    const { container, submit } = await openPanel("Rename");
    submit("bad name");
    expect(container.textContent).toContain(INVALID);
    expect(renameProfile).not.toHaveBeenCalled();
  });

  it("renames and selects the new name", async () => {
    const { submit, onSelect } = await openPanel("Rename");
    submit("clients");
    await waitFor(() => expect(renameProfile).toHaveBeenCalledWith("work", "clients"));
    expect(onSelect).toHaveBeenCalledWith("clients");
  });
});

it("deletes only after confirm() returns true", async () => {
  const confirmSpy = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);
  render(<ProfileSelector selectedProfile="work" onSelect={vi.fn()} />);
  const deleteBtn = await screen.findByText("Delete");
  fireEvent.click(deleteBtn);
  expect(deleteProfile).not.toHaveBeenCalled();
  fireEvent.click(deleteBtn);
  await waitFor(() => expect(deleteProfile).toHaveBeenCalledWith("work"));
  confirmSpy.mockRestore();
});
