// @vitest-environment jsdom
//
// Contract test for the Profiles page write path: even though the profile
// settings GET returns a `hooks` section (unfiltered on reads), no PATCH
// the page issues may ever carry it. Also covers the basic list + detail
// render and the description payload shape.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { ProfilesPage } from "../ProfilesPage";

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
  if (url === "/api/profiles/work/settings" && method === "GET") {
    // The GET deliberately includes hooks; the page must never echo them
    // back on a write.
    return jsonResponse({
      description: "",
      hooks: { on_create: ["echo seeded"] },
    });
  }
  if (url === "/api/settings" || url.startsWith("/api/settings?")) {
    return jsonResponse({ hooks: { on_launch: ["echo global"] } });
  }
  if (url === "/api/profiles/work/settings" && method === "PATCH") {
    return jsonResponse({ ok: true });
  }
  return new Response("", { status: 404 });
}

beforeEach(() => {
  fetchSpy.mockReset();
  fetchSpy.mockImplementation((input, init) =>
    Promise.resolve(route(String(input), init)),
  );
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function mount() {
  return render(
    <MemoryRouter>
      <ProfilesPage onClose={() => {}} />
    </MemoryRouter>,
  );
}

describe("ProfilesPage", () => {
  it("lists profiles with a default badge", async () => {
    const { getByRole, getByText } = mount();
    await waitFor(() => getByRole("button", { name: "work" }));
    expect(getByText("default")).toBeTruthy();
  });

  it("shows the read-only hooks panel for the selected profile", async () => {
    const { getByRole, getByText } = mount();
    await waitFor(() => getByRole("button", { name: "work" }));
    fireEvent.click(getByRole("button", { name: "work" }));
    await waitFor(() => getByText("Lifecycle hooks"));
    // Profile override is rendered, inherited global is too.
    await waitFor(() => getByText("echo seeded"));
    expect(getByText("echo global")).toBeTruthy();
  });

  it("saves a description with a body containing only `description`, never hooks", async () => {
    const { getByRole, getByPlaceholderText } = mount();
    await waitFor(() => getByRole("button", { name: "work" }));
    fireEvent.click(getByRole("button", { name: "work" }));
    await waitFor(() => getByPlaceholderText("What this profile is for"));

    fireEvent.change(getByPlaceholderText("What this profile is for"), {
      target: { value: "client repos" },
    });
    fireEvent.click(getByRole("button", { name: "Save" }));

    let patchBody: Record<string, unknown> | null = null;
    await waitFor(() => {
      const patch = fetchSpy.mock.calls.find(
        ([url, init]) =>
          String(url) === "/api/profiles/work/settings" &&
          init?.method === "PATCH",
      );
      expect(patch).toBeTruthy();
      patchBody = JSON.parse(patch![1]!.body as string);
    });
    expect(patchBody).toEqual({ description: "client repos" });
    expect(patchBody).not.toHaveProperty("hooks");
  });

  it("never sends a PATCH that includes hooks across any interaction", () => {
    mount();
    const sawHooks = fetchSpy.mock.calls.some(([url, init]) => {
      if (init?.method !== "PATCH") return false;
      if (!String(url).includes("/settings")) return false;
      return (init.body as string)?.includes("hooks");
    });
    expect(sawHooks).toBe(false);
  });
});
