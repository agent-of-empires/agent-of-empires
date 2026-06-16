// @vitest-environment jsdom
//
// Contract + interaction tests for the Profiles settings section. The
// security-critical invariant: even though the profile settings GET returns a
// `hooks` section (unfiltered on reads), no PATCH the section issues may ever
// carry it. Also exercises the CRUD / set-default / deep-link / read-only
// paths so the section's handlers are covered without a live backend.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { ProfilesSection } from "../ProfilesSection";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const fetchSpy = vi.fn<typeof fetch>();

function route(url: string, init?: RequestInit): Response {
  const method = init?.method ?? "GET";
  if (url === "/api/profiles" && method === "GET") {
    return jsonResponse([
      { name: "main", is_default: true },
      { name: "work", is_default: false, description: "" },
    ]);
  }
  if (url === "/api/profiles" && method === "POST") {
    return jsonResponse({ ok: true });
  }
  if (/^\/api\/profiles\/[^/]+\/rename$/.test(url) && method === "PATCH") {
    return jsonResponse({ ok: true });
  }
  if (/^\/api\/profiles\/[^/]+$/.test(url) && method === "DELETE") {
    return jsonResponse({ ok: true });
  }
  if (url === "/api/default-profile" && method === "PATCH") {
    return jsonResponse({ ok: true });
  }
  if (/^\/api\/profiles\/[^/]+\/settings$/.test(url) && method === "GET") {
    // The GET deliberately includes hooks; the page must never echo them
    // back on a write.
    return jsonResponse({
      description: "",
      hooks: { on_create: ["echo seeded"] },
    });
  }
  if (/^\/api\/profiles\/[^/]+\/settings$/.test(url) && method === "PATCH") {
    return jsonResponse({ ok: true });
  }
  if (url === "/api/settings" || url.startsWith("/api/settings?")) {
    return jsonResponse({ hooks: { on_launch: ["echo global"] } });
  }
  return new Response("", { status: 404 });
}

function findCall(predicate: (url: string, init?: RequestInit) => boolean) {
  return fetchSpy.mock.calls.find(([url, init]) => predicate(String(url), init as RequestInit | undefined));
}

beforeEach(() => {
  fetchSpy.mockReset();
  fetchSpy.mockImplementation((input, init) => Promise.resolve(route(String(input), init)));
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function LocationProbe() {
  const loc = useLocation();
  return <div data-testid="loc">{loc.pathname + loc.search}</div>;
}

function mount(props: { readOnly?: boolean } = {}) {
  return render(
    <MemoryRouter initialEntries={["/settings/profiles"]}>
      <ProfilesSection readOnly={props.readOnly} />
      <LocationProbe />
    </MemoryRouter>,
  );
}

// Click the profile-rail row for `name`; exact avoids matching the
// "Worktree ->" edit button (substring of "work").
async function selectWork(api: ReturnType<typeof mount>) {
  await waitFor(() => api.getByRole("button", { name: "work", exact: true }));
  fireEvent.click(api.getByRole("button", { name: "work", exact: true }));
}

describe("ProfilesSection", () => {
  it("lists profiles with a default badge", async () => {
    const api = mount();
    await waitFor(() => api.getByRole("button", { name: "work", exact: true }));
    expect(api.getByText("default")).toBeTruthy();
  });

  it("shows the read-only hooks panel for the selected profile", async () => {
    const api = mount();
    await selectWork(api);
    await waitFor(() => api.getByText("Lifecycle hooks"));
    await waitFor(() => api.getByText("echo seeded"));
    expect(api.getByText("echo global")).toBeTruthy();
  });

  it("saves a description with a body containing only `description`, never hooks", async () => {
    const api = mount();
    await selectWork(api);
    await waitFor(() => api.getByPlaceholderText("What this profile is for"));

    fireEvent.change(api.getByPlaceholderText("What this profile is for"), {
      target: { value: "client repos" },
    });
    fireEvent.click(api.getByRole("button", { name: "Save" }));

    let patchBody: Record<string, unknown> | null = null;
    await waitFor(() => {
      const patch = findCall((url, init) => url === "/api/profiles/work/settings" && init?.method === "PATCH");
      expect(patch).toBeTruthy();
      patchBody = JSON.parse(patch![1]!.body as string);
    });
    expect(patchBody).toEqual({ description: "client repos" });
    expect(patchBody).not.toHaveProperty("hooks");
  });

  it("creates a profile via + New profile (POST /api/profiles)", async () => {
    const api = mount();
    await waitFor(() => api.getByRole("button", { name: "work", exact: true }));
    fireEvent.click(api.getByRole("button", { name: "+ New profile" }));
    fireEvent.change(api.getByPlaceholderText("Profile name"), {
      target: { value: "qa" },
    });
    fireEvent.click(api.getByRole("button", { name: "Create" }));

    await waitFor(() => {
      const post = findCall((url, init) => url === "/api/profiles" && init?.method === "POST");
      expect(post).toBeTruthy();
      expect(JSON.parse(post![1]!.body as string)).toEqual({ name: "qa" });
    });
  });

  it("renames the selected profile (PATCH .../rename)", async () => {
    const api = mount();
    await selectWork(api);
    fireEvent.click(api.getByRole("button", { name: "Rename" }));
    const renameInput = api.getByPlaceholderText("New name");
    fireEvent.change(renameInput, { target: { value: "clients" } });
    fireEvent.keyDown(renameInput, { key: "Enter" });

    await waitFor(() => {
      const patch = findCall((url, init) => url === "/api/profiles/work/rename" && init?.method === "PATCH");
      expect(patch).toBeTruthy();
      expect(JSON.parse(patch![1]!.body as string)).toEqual({
        new_name: "clients",
      });
    });
  });

  it("deletes the selected profile after confirm (DELETE)", async () => {
    vi.stubGlobal("confirm", () => true);
    const api = mount();
    await selectWork(api);
    fireEvent.click(api.getByRole("button", { name: "Delete" }));

    await waitFor(() => {
      const del = findCall((url, init) => url === "/api/profiles/work" && init?.method === "DELETE");
      expect(del).toBeTruthy();
    });
  });

  it("sets the selected profile as default (PATCH /api/default-profile)", async () => {
    const api = mount();
    await selectWork(api);
    fireEvent.click(api.getByRole("button", { name: "Set as default" }));

    await waitFor(() => {
      const patch = findCall((url, init) => url === "/api/default-profile" && init?.method === "PATCH");
      expect(patch).toBeTruthy();
      expect(JSON.parse(patch![1]!.body as string)).toEqual({ name: "work" });
    });
  });

  it("deep-links into Settings scoped to the profile", async () => {
    const api = mount();
    await selectWork(api);
    fireEvent.click(api.getByRole("button", { name: /^Worktree/ }));
    await waitFor(() => expect(api.getByTestId("loc").textContent).toBe("/settings/worktree?profile=work"));
  });

  it("hides mutation controls in read-only mode", async () => {
    const api = mount({ readOnly: true });
    await selectWork(api);
    expect(api.queryByRole("button", { name: "+ New profile" })).toBeNull();
    expect(api.queryByRole("button", { name: "Set as default" })).toBeNull();
    expect(api.queryByRole("button", { name: "Rename" })).toBeNull();
    expect(api.queryByRole("button", { name: "Save" })).toBeNull();
  });

  it("never sends a PATCH that includes hooks across any interaction", () => {
    mount();
    const sawHooks = fetchSpy.mock.calls.some(([url, init]) => {
      if ((init as RequestInit | undefined)?.method !== "PATCH") return false;
      if (!String(url).includes("/settings")) return false;
      return ((init as RequestInit).body as string)?.includes("hooks");
    });
    expect(sawHooks).toBe(false);
  });

  // Regression: selecting a profile loads its settings asynchronously and the
  // load's `.then` writes the description field. A user who starts typing
  // before that resolves must keep their edit; the late load must not reset
  // the field (which previously made Save PATCH the loaded value, or null).
  it("keeps a description edit made while the profile load is still in flight", async () => {
    // Gate every settings GET so the load stays pending while we type.
    let releaseLoad: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      releaseLoad = resolve;
    });
    fetchSpy.mockImplementation((input, init) => {
      const url = String(input);
      const method = (init as RequestInit | undefined)?.method ?? "GET";
      if (/^\/api\/profiles\/[^/]+\/settings$/.test(url) && method === "GET") {
        return gate.then(() => jsonResponse({ description: "from-server", hooks: {} }));
      }
      return Promise.resolve(route(url, init as RequestInit | undefined));
    });

    const api = mount();
    await selectWork(api);
    const field = (await waitFor(() => api.getByPlaceholderText("What this profile is for"))) as HTMLInputElement;

    // Edit while the settings GET is still pending.
    fireEvent.change(field, { target: { value: "client repos" } });
    expect(field.value).toBe("client repos");

    // Resolve the load. "echo global" (from the global-settings GET) renders
    // only after the load's `.then` runs, so it is a deterministic signal
    // that the load applied and React flushed.
    releaseLoad();
    await waitFor(() => api.getByText("echo global"));

    // The edit survives; the loaded "from-server" value never reaches it.
    expect(field.value).toBe("client repos");
    expect(api.queryByDisplayValue("from-server")).toBeNull();

    // Save must carry the user's value, not the clobbered/empty one.
    fireEvent.click(api.getByRole("button", { name: "Save" }));
    await waitFor(() => {
      const patch = findCall((url, init) => url === "/api/profiles/work/settings" && init?.method === "PATCH");
      expect(patch).toBeTruthy();
      expect(JSON.parse(patch![1]!.body as string)).toEqual({
        description: "client repos",
      });
    });
  });
});
