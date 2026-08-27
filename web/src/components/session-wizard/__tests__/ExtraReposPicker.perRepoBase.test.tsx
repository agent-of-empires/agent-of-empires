// @vitest-environment jsdom
//
// Per-repo base branches at session creation (#3329). Before this, the wizard
// held one `baseBranch` for the whole session, so a workspace where one repo
// forks from develop and the others from their own epic branches could not be
// created; users made everything on develop and re-pointed repos by hand.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ExtraReposPicker } from "../steps/ExtraReposPicker";

const fetchBranches = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchProjects: vi.fn().mockResolvedValue([]),
  fetchBranches: (...args: unknown[]) => fetchBranches(...args),
}));

beforeEach(() => {
  fetchBranches.mockReset();
  fetchBranches.mockResolvedValue([]);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function renderPicker(over: { repoBases?: Record<string, string>; basesEnabled?: boolean } = {}) {
  const onChange = vi.fn();
  const onRepoBasesChange = vi.fn();
  const utils = render(
    <ExtraReposPicker
      primaryPath="/src/app"
      selectedPaths={["/src/api", "/src/web"]}
      onChange={onChange}
      repoBases={over.repoBases ?? {}}
      onRepoBasesChange={onRepoBasesChange}
      basesEnabled={over.basesEnabled ?? true}
    />,
  );
  return { onChange, onRepoBasesChange, ...utils };
}

describe("ExtraReposPicker per-repo base branch (#3329)", () => {
  it("reports a base keyed by the repo it was typed for, leaving siblings alone", () => {
    const { onRepoBasesChange } = renderPicker();

    fireEvent.change(screen.getByLabelText("Base branch for api"), {
      target: { value: "epic/checkout" },
    });

    expect(onRepoBasesChange).toHaveBeenCalledWith({ "/src/api": "epic/checkout" });
  });

  it("keeps existing entries when a second repo gets its own base", () => {
    const { onRepoBasesChange } = renderPicker({ repoBases: { "/src/api": "epic/checkout" } });

    fireEvent.change(screen.getByLabelText("Base branch for web"), { target: { value: "develop" } });

    expect(onRepoBasesChange).toHaveBeenCalledWith({
      "/src/api": "epic/checkout",
      "/src/web": "develop",
    });
  });

  it("drops the entry when the field is cleared, so the repo falls back to the session base", () => {
    const { onRepoBasesChange } = renderPicker({
      repoBases: { "/src/api": "epic/checkout", "/src/web": "develop" },
    });

    fireEvent.change(screen.getByLabelText("Base branch for api"), { target: { value: "" } });

    expect(onRepoBasesChange).toHaveBeenCalledWith({ "/src/web": "develop" });
  });

  it("drops the entry when the repo is removed, so a stale base cannot be submitted", () => {
    const { onChange, onRepoBasesChange } = renderPicker({
      repoBases: { "/src/api": "epic/checkout" },
    });

    fireEvent.click(screen.getByLabelText("Remove api"));

    expect(onChange).toHaveBeenCalledWith(["/src/web"]);
    expect(onRepoBasesChange).toHaveBeenCalledWith({});
  });

  it("suggests branches from the repo being edited, not the launch repo", async () => {
    fetchBranches.mockResolvedValue([{ name: "epic/checkout", is_current: false }]);
    renderPicker();

    fireEvent.focus(screen.getByLabelText("Base branch for web"));

    await waitFor(() => expect(fetchBranches).toHaveBeenCalledWith("/src/web", true));
    expect(fetchBranches).not.toHaveBeenCalledWith("/src/app", true);
    fireEvent.mouseDown(await screen.findByText("epic/checkout"));
  });

  it("hides the inputs while attaching to an existing branch, which has no base to fork from", () => {
    renderPicker({ basesEnabled: false });
    expect(screen.queryByLabelText("Base branch for api")).toBeNull();
    // The repos themselves are still listed and removable.
    expect(screen.getByLabelText("Remove api")).toBeTruthy();
  });
});
