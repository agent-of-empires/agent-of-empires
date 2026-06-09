// @vitest-environment jsdom
//
// Client-side name-validation contract for ProfileSelector. Mirrors the
// server-side `validate_profile_name` rules (alphanumeric + `_-`, non-empty,
// <=64 chars) without round-tripping a real `aoe serve`. Mocks the api
// module so each assertion can pin whether the network was ever touched,
// which is the property the validator must guarantee.
//
// The lifecycle round-trip through the full app lives in the mocked
// Playwright spec `web/tests/profile-lifecycle.spec.ts`. This test focuses on
// the validation branches that don't need a browser.

import { afterEach, describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { ProfileSelector } from "../ProfileSelector";

vi.mock("../../../lib/api", () => ({
  fetchProfiles: vi.fn(),
  createProfile: vi.fn(),
  renameProfile: vi.fn(),
  deleteProfile: vi.fn(),
}));

import { fetchProfiles, createProfile, renameProfile, deleteProfile } from "../../../lib/api";

const mockFetch = vi.mocked(fetchProfiles);
const mockCreate = vi.mocked(createProfile);
const mockRename = vi.mocked(renameProfile);
const mockDelete = vi.mocked(deleteProfile);

beforeEach(() => {
  vi.clearAllMocks();
  mockFetch.mockResolvedValue([{ name: "default", is_default: true }]);
  mockCreate.mockResolvedValue(true);
  mockRename.mockResolvedValue(true);
  mockDelete.mockResolvedValue(true);
});

afterEach(() => {
  // Vitest doesn't enable RTL auto-cleanup (no setupFiles in vite.config.ts),
  // so each render leaks DOM into the next test. Without this, queries
  // like document.querySelector and getByText match multiple ProfileSelector
  // instances stacked on top of each other.
  cleanup();
});

function mount() {
  const onSelect = vi.fn();
  const utils = render(<ProfileSelector selectedProfile="default" onSelect={onSelect} />);
  return { onSelect, ...utils };
}

async function openCreatePanel(getByText: (t: string) => HTMLElement) {
  const newBtn = getByText("+ New");
  fireEvent.click(newBtn);
  // Wait for the input to appear before tests fill it.
  await waitFor(() => {
    const input = document.querySelector('input[placeholder="Profile name"]') as HTMLInputElement | null;
    if (!input) throw new Error("create input not mounted");
  });
  return document.querySelector('input[placeholder="Profile name"]') as HTMLInputElement;
}

function submit(input: HTMLInputElement) {
  fireEvent.keyDown(input, { key: "Enter", code: "Enter" });
}

describe("ProfileSelector create-name validation", () => {
  it("whitespace-only name shows 'Name is required' and never calls createProfile", async () => {
    const { getByText, container } = mount();
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    const input = await openCreatePanel(getByText);
    fireEvent.change(input, { target: { value: "   " } });
    submit(input);

    expect(container.textContent).toContain("Name is required");
    expect(mockCreate).not.toHaveBeenCalled();
  });

  it("empty input shows 'Name is required'", async () => {
    const { getByText, container } = mount();
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    const input = await openCreatePanel(getByText);
    fireEvent.change(input, { target: { value: "" } });
    submit(input);

    expect(container.textContent).toContain("Name is required");
    expect(mockCreate).not.toHaveBeenCalled();
  });

  it.each([
    ["bad name", "space"],
    ["bad;name", "semicolon"],
    ["bad$name", "dollar"],
    ["bad|name", "pipe"],
    ["bad&name", "ampersand"],
    ["bad`name", "backtick"],
    ["bad/name", "slash"],
    ["..", "double-dot"],
    [".hidden", "leading dot"],
  ])("rejects %s (%s) without calling createProfile", async (bad) => {
    const { getByText, container } = mount();
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    const input = await openCreatePanel(getByText);
    fireEvent.change(input, { target: { value: bad } });
    submit(input);

    expect(container.textContent).toContain("Only letters, digits, hyphens, and underscores");
    expect(mockCreate).not.toHaveBeenCalled();
  });

  it.each([["work"], ["work-2"], ["work_2"], ["A"], ["my-profile_42"]])(
    "accepts %s and calls createProfile with the trimmed value",
    async (good) => {
      const { getByText } = mount();
      await waitFor(() => expect(mockFetch).toHaveBeenCalled());

      const input = await openCreatePanel(getByText);
      fireEvent.change(input, { target: { value: `  ${good}  ` } });
      submit(input);

      await waitFor(() => expect(mockCreate).toHaveBeenCalledWith(good));
    },
  );

  it("createProfile failure surfaces 'Failed to create profile'", async () => {
    mockCreate.mockResolvedValueOnce(false);
    const { getByText, container } = mount();
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    const input = await openCreatePanel(getByText);
    fireEvent.change(input, { target: { value: "duplicate" } });
    submit(input);

    await waitFor(() => expect(container.textContent).toContain("Failed to create profile"));
  });
});

describe("ProfileSelector rename validation", () => {
  it("rename to same name closes the panel without calling renameProfile", async () => {
    mockFetch.mockResolvedValue([
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ]);
    const { container } = render(<ProfileSelector selectedProfile="work" onSelect={vi.fn()} />);
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    fireEvent.click(
      Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Rename") as HTMLButtonElement,
    );
    const input = (await waitFor(() => {
      const el = document.querySelector('input[placeholder="New name"]') as HTMLInputElement | null;
      if (!el) throw new Error("rename input not mounted");
      return el;
    })) as HTMLInputElement;

    // Input pre-fills with selectedProfile. Submit without changing.
    expect(input.value).toBe("work");
    submit(input);

    expect(mockRename).not.toHaveBeenCalled();
    expect(document.querySelector('input[placeholder="New name"]')).toBeNull();
  });

  it("rename to invalid name shows error and skips API call", async () => {
    mockFetch.mockResolvedValue([
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ]);
    const { container } = render(<ProfileSelector selectedProfile="work" onSelect={vi.fn()} />);
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    fireEvent.click(
      Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Rename") as HTMLButtonElement,
    );
    const input = (await waitFor(() => {
      const el = document.querySelector('input[placeholder="New name"]') as HTMLInputElement | null;
      if (!el) throw new Error("rename input not mounted");
      return el;
    })) as HTMLInputElement;

    fireEvent.change(input, { target: { value: "bad name" } });
    submit(input);

    expect(container.textContent).toContain("Only letters, digits, hyphens, and underscores");
    expect(mockRename).not.toHaveBeenCalled();
  });

  it("rename to a valid new name calls renameProfile(old, new) and notifies parent", async () => {
    mockFetch.mockResolvedValue([
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ]);
    const onSelect = vi.fn();
    const { container } = render(<ProfileSelector selectedProfile="work" onSelect={onSelect} />);
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    fireEvent.click(
      Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Rename") as HTMLButtonElement,
    );
    const input = (await waitFor(() => {
      const el = document.querySelector('input[placeholder="New name"]') as HTMLInputElement | null;
      if (!el) throw new Error("rename input not mounted");
      return el;
    })) as HTMLInputElement;

    fireEvent.change(input, { target: { value: "clients" } });
    submit(input);

    await waitFor(() => expect(mockRename).toHaveBeenCalledWith("work", "clients"));
    expect(onSelect).toHaveBeenCalledWith("clients");
  });
});

describe("ProfileSelector delete confirm gating", () => {
  it("delete only calls deleteProfile after confirm() returns true", async () => {
    mockFetch.mockResolvedValue([
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ]);
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);

    const onSelect = vi.fn();
    const { container } = render(<ProfileSelector selectedProfile="work" onSelect={onSelect} />);
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());

    const deleteBtn = await waitFor(
      () =>
        Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Delete") as HTMLButtonElement,
    );

    // First click: confirm() returns false -> no API call.
    fireEvent.click(deleteBtn);
    expect(mockDelete).not.toHaveBeenCalled();

    // Second click: confirm() returns true -> deleteProfile fires.
    fireEvent.click(deleteBtn);
    await waitFor(() => expect(mockDelete).toHaveBeenCalledWith("work"));

    confirmSpy.mockRestore();
  });
});
