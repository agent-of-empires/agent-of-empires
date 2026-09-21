// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { ProfilesSection } from "../ProfilesSection";

const jsonResponse = (body: unknown) =>
  new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });

const fetchSpy = vi.fn<typeof fetch>();
const SETTINGS_URL = /^\/api\/profiles\/[^/]+\/settings$/;

function route(url: string, init?: RequestInit): Response {
  const method = init?.method ?? "GET";
  if (url === "/api/profiles" && method === "GET") {
    return jsonResponse([
      { name: "main", is_default: true },
      { name: "work", is_default: false, description: "" },
    ]);
  }
  if (SETTINGS_URL.test(url) && method === "GET") {
    return jsonResponse({ description: "", hooks: { on_create: ["echo seeded"] } });
  }
  if (url === "/api/settings" || url.startsWith("/api/settings?")) {
    return jsonResponse({ hooks: { on_launch: ["echo global"] } });
  }
  if (method !== "GET") return jsonResponse({ ok: true });
  return new Response("", { status: 404 });
}

function findCall(url: string, method: string) {
  return fetchSpy.mock.calls.find(([u, init]) => String(u) === url && init?.method === method);
}

async function expectBody(url: string, method: string, body?: unknown) {
  await waitFor(() => {
    const call = findCall(url, method);
    expect(call).toBeTruthy();
    if (body !== undefined) expect(JSON.parse(call![1]!.body as string)).toEqual(body);
  });
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

// `exact` so "work" does not match the "Worktree ->" button.
async function mountAndSelectWork(readOnly?: boolean) {
  const api = render(
    <MemoryRouter initialEntries={["/settings/profiles"]}>
      <ProfilesSection readOnly={readOnly} />
      <LocationProbe />
    </MemoryRouter>,
  );
  const work = await waitFor(() => api.getByRole("button", { name: "work", exact: true }));
  fireEvent.click(work);
  return api;
}

describe("ProfilesSection", () => {
  it("lists profiles with a default badge and shows the selected profile's hooks", async () => {
    const api = await mountAndSelectWork();
    expect(api.getByText("default")).toBeTruthy();
    await waitFor(() => api.getByText("echo seeded"));
    expect(api.getByText("echo global")).toBeTruthy();
  });

  it("saves a description with only `description` in the body, never hooks", async () => {
    const api = await mountAndSelectWork();
    fireEvent.change(await waitFor(() => api.getByPlaceholderText("What this profile is for")), {
      target: { value: "client repos" },
    });
    fireEvent.click(api.getByRole("button", { name: "Save" }));
    await expectBody("/api/profiles/work/settings", "PATCH", { description: "client repos" });
  });

  it.each<[string, (api: Awaited<ReturnType<typeof mountAndSelectWork>>) => void, string, string, unknown]>([
    [
      "creates a profile",
      (api) => {
        fireEvent.click(api.getByRole("button", { name: "+ New profile" }));
        fireEvent.change(api.getByPlaceholderText("Profile name"), { target: { value: "qa" } });
        fireEvent.click(api.getByRole("button", { name: "Create" }));
      },
      "/api/profiles",
      "POST",
      { name: "qa" },
    ],
    [
      "renames the selected profile on Enter",
      (api) => {
        fireEvent.click(api.getByRole("button", { name: "Rename" }));
        const input = api.getByPlaceholderText("New name");
        fireEvent.change(input, { target: { value: "clients" } });
        fireEvent.keyDown(input, { key: "Enter" });
      },
      "/api/profiles/work/rename",
      "PATCH",
      { new_name: "clients" },
    ],
    [
      "deletes the selected profile after confirm",
      (api) => {
        vi.stubGlobal("confirm", () => true);
        fireEvent.click(api.getByRole("button", { name: "Delete" }));
      },
      "/api/profiles/work",
      "DELETE",
      undefined,
    ],
    [
      "sets the selected profile as default",
      (api) => fireEvent.click(api.getByRole("button", { name: "Set as default" })),
      "/api/default-profile",
      "PATCH",
      { name: "work" },
    ],
  ])("%s", async (_, act, url, method, body) => {
    const api = await mountAndSelectWork();
    act(api);
    await expectBody(url, method, body);
  });

  it("deep-links into Settings scoped to the profile", async () => {
    const api = await mountAndSelectWork();
    fireEvent.click(api.getByRole("button", { name: /^Worktree/ }));
    await waitFor(() => expect(api.getByTestId("loc").textContent).toBe("/settings/worktree?profile=work"));
  });

  it("hides mutation controls in read-only mode", async () => {
    const api = await mountAndSelectWork(true);
    for (const name of ["+ New profile", "Set as default", "Rename", "Save"]) {
      expect(api.queryByRole("button", { name })).toBeNull();
    }
  });

  it("keeps a description edit made while the profile load is still in flight", async () => {
    let releaseLoad: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      releaseLoad = resolve;
    });
    fetchSpy.mockImplementation((input, init) => {
      const url = String(input);
      if (SETTINGS_URL.test(url) && (init?.method ?? "GET") === "GET") {
        return gate.then(() => jsonResponse({ description: "from-server", hooks: {} }));
      }
      return Promise.resolve(route(url, init));
    });

    const api = await mountAndSelectWork();
    const field = (await waitFor(() => api.getByPlaceholderText("What this profile is for"))) as HTMLInputElement;
    fireEvent.change(field, { target: { value: "client repos" } });

    releaseLoad();
    // "echo global" renders only after the load's `.then` has applied.
    await waitFor(() => api.getByText("echo global"));
    expect(field.value).toBe("client repos");
    expect(api.queryByDisplayValue("from-server")).toBeNull();

    fireEvent.click(api.getByRole("button", { name: "Save" }));
    await expectBody("/api/profiles/work/settings", "PATCH", { description: "client repos" });
  });
});
