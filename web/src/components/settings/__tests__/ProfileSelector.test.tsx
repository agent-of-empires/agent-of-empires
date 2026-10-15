// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProfileSelector } from "../ProfileSelector";
import { validateProfileName } from "../../profiles/profileName";

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
    ...["bad name", "bad;name", "bad$name", "bad|name", "bad&name", "bad`name", "bad/name", "..", ".hidden"].map(
      (n) => [n, INVALID],
    ),
  ])("validateProfileName rejects %j", (name, message) => {
    expect(validateProfileName(name)).toBe(message);
  });

  it.each(["work", "work-2", "work_2", "A", "my-profile_42"])("validateProfileName accepts %s", (good) => {
    expect(validateProfileName(good)).toBeNull();
  });

  it("rejects blank and unsafe names without calling createProfile, then creates a trimmed name", async () => {
    const { container, submit } = await openPanel("+ New");
    submit("   ");
    expect(container.textContent).toContain("Name is required");
    submit("../x");
    expect(container.textContent).toContain(INVALID);
    expect(createProfile).not.toHaveBeenCalled();
    submit("  work-2  ");
    await waitFor(() => expect(createProfile).toHaveBeenCalledWith("work-2"));
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

  it("rejects an invalid name, then renames and selects a valid one", async () => {
    const { container, submit, onSelect } = await openPanel("Rename");
    submit("bad/name");
    expect(container.textContent).toContain(INVALID);
    expect(renameProfile).not.toHaveBeenCalled();
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
