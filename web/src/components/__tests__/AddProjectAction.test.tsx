// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { jsonResponse, makeSession, makeWorkspace, openRowMenu, renderRow, stubFetch } from "./fixtures";

const ws = (over: Partial<SessionResponse> = {}) => makeWorkspace("w1", [makeSession({ id: "sess-9", ...over })]);
const ATTACH_URL = "/api/sessions/sess-9/projects";

function attachOk(worker: string, extra: Record<string, unknown> = {}) {
  return {
    session: null,
    attached: {
      name: "frontend",
      branch: "feature/abc",
      branch_created: true,
      moved_to: "/src/feature-abc-workspace-abcd1234",
    },
    warnings: [],
    worker,
    worker_message: null,
    ...extra,
  };
}

let fetchSpy: ReturnType<typeof stubFetch>;
/** Answers the picker's fetches, and every attach POST with `attach()`. */
function mockAttach(attach: () => Response | Promise<Response>, recent: { path: string; display_name: string }[] = []) {
  fetchSpy.mockImplementation(async (input) => {
    const url = String(input);
    if (url.includes(ATTACH_URL)) return attach();
    if (url.includes("/api/projects"))
      return jsonResponse([{ name: "frontend", path: "/src/frontend", pinned: false, scope: "global" }]);
    return jsonResponse(
      url.includes("/api/recent-projects")
        ? { projects: recent.map((r) => ({ ...r, tool: "claude", last_used_at: "2025-01-02T00:00:00Z" })) }
        : { sessions: [] },
    );
  });
}
const attachCalls = () => fetchSpy.mock.calls.filter(([url]) => String(url).includes(ATTACH_URL));

beforeEach(() => {
  fetchSpy = stubFetch();
  mockAttach(() => jsonResponse(attachOk("restarted")));
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function submit(value: string | null, { reuseBranch = false } = {}) {
  openRowMenu(ws());
  fireEvent.click(screen.getByTestId("sidebar-context-menu-add-project"));
  await waitFor(() => expect(screen.queryByTestId("add-project-modal")).not.toBeNull());
  if (value != null) fireEvent.change(screen.getByTestId("add-project-modal-input"), { target: { value } });
  if (reuseBranch) fireEvent.click(screen.getByTestId("add-project-modal-attach-existing-branch"));
  fireEvent.click(screen.getByTestId("add-project-modal-submit"));
}

describe("SessionRow Add project entry", () => {
  it.each([
    // Running is decided by the server's turn probe; a 409 surfaces in the modal.
    ["a Running row", { status: "Running" }, {}, true],
    ["a read-only row", {}, { readOnly: true }, false],
    // The rest are refused by `attach_project::plan`.
    ["a scratch session", { scratch: true }, {}, false],
    ["a mid-create session", { status: "Creating" }, {}, false],
    ["an archived session", { archived_at: "2025-01-02T00:00:00Z" }, {}, false],
    ["a trashed session", { trashed_at: "2025-01-02T00:00:00Z" }, {}, false],
  ] as [string, Partial<SessionResponse>, { readOnly?: boolean }, boolean][])(
    "on %s: offered=%s",
    (_n, over, options, offered) => {
      openRowMenu(ws(over), options);
      expect(screen.queryByTestId("sidebar-context-menu-add-project") != null).toBe(offered);
      // The menu opened, so a hidden entry is the per-entry gate.
      expect(screen.queryByTestId("sidebar-context-menu-rename")).not.toBeNull();
    },
  );

  it("is unreachable on a mid-delete row, which opens no menu", () => {
    renderRow(ws({ status: "Deleting" }));
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
  });
});

describe("AddProjectModal", () => {
  it.each([
    ["frontend", false],
    ["/src/frontend", true],
    ["/tmp/unlisted-project", false],
  ])("posts %j with attach_existing_branch=%s", async (project, reuseBranch) => {
    await submit(project, { reuseBranch });
    await waitFor(() => expect(attachCalls()).toHaveLength(1));
    const init = attachCalls()[0]![1] as RequestInit;
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({ project, attach_existing_branch: reuseBranch });
  });

  it("lists saved projects in the picker instead of a native datalist", async () => {
    await submit(null);
    await waitFor(() => expect(screen.queryByTitle("/src/frontend")).not.toBeNull());
    expect(screen.getByTestId("add-project-modal").querySelector("datalist")).toBeNull();
  });

  it("posts the path of a clicked picker row", async () => {
    openRowMenu(ws());
    fireEvent.click(screen.getByTestId("sidebar-context-menu-add-project"));
    fireEvent.click(await waitFor(() => screen.getByTitle("/src/frontend")));
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));
    await waitFor(() => expect(attachCalls()).toHaveLength(1));
    expect(JSON.parse((attachCalls()[0]![1] as RequestInit).body as string)).toEqual({
      project: "/src/frontend",
      attach_existing_branch: false,
    });
  });

  it("lists a recent-only project and posts its path when clicked", async () => {
    mockAttach(() => jsonResponse(attachOk("restarted")), [{ path: "/src/recent-only", display_name: "recent-only" }]);
    openRowMenu(ws());
    fireEvent.click(screen.getByTestId("sidebar-context-menu-add-project"));
    fireEvent.click(await waitFor(() => screen.getByTitle("/src/recent-only")));
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));
    await waitFor(() => expect(attachCalls()).toHaveLength(1));
    expect(JSON.parse((attachCalls()[0]![1] as RequestInit).body as string)).toEqual({
      project: "/src/recent-only",
      attach_existing_branch: false,
    });
  });

  it("does not post an empty project", async () => {
    await submit(null);
    await waitFor(() => expect(screen.queryByTestId("add-project-modal-error")).not.toBeNull());
    expect(attachCalls()).toHaveLength(0);
  });

  it("ignores Escape and backdrop clicks while the attach is in flight", async () => {
    let release: (v: Response) => void = () => {};
    mockAttach(() => new Promise<Response>((resolve) => (release = resolve)));
    await submit("frontend");
    await waitFor(() => expect(screen.getByTestId("add-project-modal-submit").textContent).toContain("Attaching"));

    fireEvent.keyDown(document, { key: "Escape" });
    fireEvent.click(screen.getByTestId("add-project-modal-backdrop"));
    expect(screen.queryByTestId("add-project-modal")).not.toBeNull();

    release(jsonResponse(attachOk("restarted")));
    await waitFor(() => expect(screen.queryByTestId("add-project-modal-result")).not.toBeNull());
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("add-project-modal")).toBeNull());
  });

  it("reports the new working directory and a failed restart", async () => {
    mockAttach(() => jsonResponse(attachOk("restart_failed", { worker_message: "worker respawn failed: boom" })));
    await submit("frontend");
    const result = await waitFor(() => screen.getByTestId("add-project-modal-result"));
    expect(result.textContent).toContain("frontend");
    expect(screen.getByTestId("add-project-modal-moved-to").textContent).toContain(
      "/src/feature-abc-workspace-abcd1234",
    );
    expect(screen.getByTestId("add-project-modal").textContent).toContain("did not restart");
  });

  it("surfaces the server's refusal message", async () => {
    mockAttach(() => jsonResponse({ message: "branch 'feature/abc' already exists in the repo being attached" }, 400));
    await submit("frontend");
    const err = await waitFor(() => screen.getByTestId("add-project-modal-error"));
    expect(err.textContent).toContain("already exists");
  });
});
