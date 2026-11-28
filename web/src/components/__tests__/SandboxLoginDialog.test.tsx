// @vitest-environment jsdom

import { StrictMode } from "react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi, type MockInstance } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";

import { writeClipboard } from "../../lib/clipboard";
import { SandboxLoginDialog, SandboxLoginStatusRow } from "../SandboxLoginDialog";

vi.mock("../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));

const URL_ONE = "https://login.example.test/authorize?code=true&state=SAMPLE-ONE";
const URL_TWO = "https://login.example.test/authorize?code=true&state=SAMPLE-TWO";
const CODE = "SAMPLE-CODE-1234#SAMPLE-STATE";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterAll(() => server.close());

// Unmounting cancels a live login with a request that outlives the test. Settle it before the
// next test registers handlers, or it lands in that test's `cancels`.
const cancelsInFlight = new Set<Promise<unknown>>();
let fetchSpy: MockInstance<typeof fetch>;
beforeEach(() => {
  const realFetch = globalThis.fetch;
  fetchSpy = vi.spyOn(globalThis, "fetch").mockImplementation((input, init) => {
    const request = realFetch(input, init);
    if (init?.method === "DELETE") cancelsInFlight.add(request);
    return request;
  });
});
afterEach(async () => {
  cleanup();
  await Promise.allSettled([...cancelsInFlight]);
  cancelsInFlight.clear();
  fetchSpy.mockRestore();
  server.resetHandlers();
});

interface Calls {
  starts: unknown[];
  codes: { loginId: string; body: unknown }[];
  cancels: string[];
  /** The lease each cancel carried, in order. */
  leases: (string | null)[];
  /** The lease each start named, in order; `starts` leaves it out. */
  startLeases: unknown[];
  /** The lease of each leave by lease alone, which names no login. */
  leaves: (string | null)[];
}

/** A start's body without its lease, which is random: the lease goes to `startLeases`. */
function recordStart(calls: Calls, body: unknown) {
  const { lease, ...rest } = body as { lease?: unknown };
  calls.startLeases.push(lease);
  calls.starts.push(rest);
}

/** A promise the test settles by hand, to hold a request open. */
function gate() {
  let open!: () => void;
  const opened = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { opened, open };
}

function mockApi(overrides: {
  start?: () => Response | Promise<Response>;
  code?: (loginId: string) => Response | Promise<Response>;
}): Calls {
  const calls: Calls = { starts: [], codes: [], cancels: [], leases: [], startLeases: [], leaves: [] };
  let n = 0;
  server.use(
    http.delete("*/api/sandbox/login", ({ request }) => {
      calls.leaves.push(new URL(request.url).searchParams.get("lease"));
      return new HttpResponse(null, { status: 204 });
    }),
    http.post("*/api/sandbox/login", async ({ request }) => {
      recordStart(calls, await request.json());
      if (overrides.start) return overrides.start();
      n += 1;
      return HttpResponse.json({ login_id: `login-${n}`, lease: `lease-${n}`, url: n === 1 ? URL_ONE : URL_TWO });
    }),
    http.post("*/api/sandbox/login/:loginId/code", async ({ request, params }) => {
      const loginId = String(params.loginId);
      calls.codes.push({ loginId, body: await request.json() });
      return overrides.code ? overrides.code(loginId) : HttpResponse.json({ ok: true });
    }),
    http.delete("*/api/sandbox/login/:loginId", ({ params, request }) => {
      calls.cancels.push(String(params.loginId));
      calls.leases.push(new URL(request.url).searchParams.get("lease"));
      return new HttpResponse(null, { status: 204 });
    }),
  );
  return calls;
}

const err = (status: number, error: string, message?: string) =>
  HttpResponse.json(message === undefined ? { error } : { error, message }, { status });

/** The server's answer to what a login's code exchange came to; returns the login ids it was asked about. */
function mockOutcome(...outcomes: string[]): string[] {
  const asked: string[] = [];
  server.use(
    http.get("*/api/sandbox/login/:loginId", ({ params }) => {
      asked.push(String(params.loginId));
      const outcome = outcomes[Math.min(asked.length, outcomes.length) - 1];
      return outcome === "error" ? err(500, "internal") : HttpResponse.json({ outcome });
    }),
  );
  return asked;
}

/** The first code's answer gets lost; later ones get `then`. */
function lostFirstAnswer(...then: (() => Response)[]) {
  let n = 0;
  return () => {
    n += 1;
    if (n === 1) return HttpResponse.error();
    return (then[n - 2] ?? then[then.length - 1])();
  };
}

const NOT_FOUND_TEXT = "That login ended. Start again for a new link.";
const ELEVATION_SUBMIT_TEXT = "Confirm your passphrase, then press Submit again.";

function codeInput(): HTMLInputElement {
  return screen.getByLabelText("Sign-in code") as HTMLInputElement;
}

function submitCode(value = CODE) {
  fireEvent.change(codeInput(), { target: { value } });
  fireEvent.click(screen.getByRole("button", { name: "Submit" }));
}

async function ready() {
  await screen.findByRole("link", { name: /sign in/i });
}

beforeEach(() => {
  vi.mocked(writeClipboard).mockClear();
});

describe("SandboxLoginDialog", () => {
  it("starts a login and shows the link with safe attributes and a hardened code field", async () => {
    const calls = mockApi({});
    render(<SandboxLoginDialog onClose={() => {}} />);

    const link = (await screen.findByRole("link", { name: /sign in/i })) as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe(URL_ONE);
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    expect(calls.starts).toEqual([{ force: false }]);

    const input = codeInput();
    expect(input.type).toBe("password");
    expect(input.getAttribute("autocomplete")).toBe("off");
    expect(input.getAttribute("spellcheck")).toBe("false");
    expect(input.getAttribute("name")).toBe("sandbox-login-code");
    expect(input.getAttribute("data-1p-ignore")).not.toBeNull();
    expect(input.getAttribute("data-lpignore")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Copy link" }));
    expect(writeClipboard).toHaveBeenCalledWith(URL_ONE);
  });

  it("starts the login of the tool it is given", async () => {
    const calls = mockApi({});
    render(<SandboxLoginDialog tool="claude-personal" onClose={() => {}} />);
    await ready();
    expect(calls.starts).toEqual([{ force: false, tool: "claude-personal" }]);
  });

  it("starts the login of the command it is given, next to its tool", async () => {
    const calls = mockApi({});
    render(<SandboxLoginDialog tool="personal" command="my claude --fast" onClose={() => {}} />);
    await ready();
    expect(calls.starts).toEqual([{ force: false, tool: "personal", command: "my claude --fast" }]);
  });

  it("starts the login of the profile it is given, next to its tool and command", async () => {
    const calls = mockApi({});
    render(<SandboxLoginDialog tool="personal" command="my claude" profile="work" onClose={() => {}} />);
    await ready();
    expect(calls.starts).toEqual([{ force: false, tool: "personal", command: "my claude", profile: "work" }]);
  });

  it("asks for the outcome of its own login when it checks a lost answer", async () => {
    mockApi({ code: lostFirstAnswer(() => err(404, "login_not_found")) });
    const asked = mockOutcome("succeeded");
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog tool="claude" profile="work" onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    expect(await screen.findByRole("alert")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(asked).toEqual(["login-1"]);
  });

  it("never renders a non-https link", async () => {
    mockApi({ start: () => HttpResponse.json({ login_id: "login-1", url: "javascript:alert(1)" }) });
    render(<SandboxLoginDialog onClose={() => {}} />);
    await screen.findByLabelText("Sign-in code");
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("submits the code, clears the field at once, then closes and refreshes on success", async () => {
    const calls = mockApi({});
    const onClose = vi.fn();
    const onLoggedIn = vi.fn();
    const { unmount } = render(<SandboxLoginDialog onClose={onClose} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    expect(codeInput().value).toBe("");
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(calls.codes).toEqual([{ loginId: "login-1", body: { code: CODE } }]);
    unmount();
    // A finished login is not cancelled, neither by closing nor by unmounting.
    expect(calls.cancels).toEqual([]);
  });

  it("sends one code for a double submit", async () => {
    const hold = gate();
    const calls = mockApi({
      code: async () => {
        await hold.opened;
        return HttpResponse.json({ ok: true });
      },
    });
    const onClose = vi.fn();
    const { container } = render(<SandboxLoginDialog onClose={onClose} />);
    await ready();

    fireEvent.change(codeInput(), { target: { value: CODE } });
    const form = container.ownerDocument.querySelector("form") as HTMLFormElement;
    fireEvent.submit(form);
    fireEvent.submit(form);
    fireEvent.click(screen.getByRole("button", { name: /submitting/i }));
    // `calls.codes` counts requests at the network boundary; wait for the login to finish before counting.
    await waitFor(() => expect(calls.codes).toHaveLength(1));
    hold.open();
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
    expect(calls.codes).toHaveLength(1);
  });

  it("keeps the link and the typed code on invalid_code and the same login takes the corrected code", async () => {
    let first = true;
    const calls = mockApi({
      code: () => {
        if (first) {
          first = false;
          return err(400, "invalid_code", "The code has a space in it.");
        }
        return HttpResponse.json({ ok: true });
      },
    });
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode("bad code");
    expect(await screen.findByText("The code has a space in it.")).toBeTruthy();
    expect(screen.getByRole("link", { name: /sign in/i })).toBeTruthy();
    expect(codeInput().value).toBe("bad code");
    expect(screen.getByRole("alert").textContent).toBe("The code has a space in it.");
    // The input takes focus again once it is enabled.
    await waitFor(() => expect(document.activeElement).toBe(codeInput()));

    submitCode("good-code");
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(calls.codes.map((c) => [c.loginId, c.body])).toEqual([
      ["login-1", { code: "bad code" }],
      ["login-1", { code: "good-code" }],
    ]);
    expect(calls.starts).toHaveLength(1);
  });

  it("hides link and field on login_failed and Start again fetches a new link that gets the next code", async () => {
    let first = true;
    const calls = mockApi({
      code: () => {
        if (first) {
          first = false;
          return err(422, "login_failed", "Login failed: Invalid code.");
        }
        return HttpResponse.json({ ok: true });
      },
    });
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    expect(await screen.findByText("Login failed: Invalid code.")).toBeTruthy();
    expect(screen.queryByRole("link")).toBeNull();
    expect(screen.queryByLabelText("Sign-in code")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Start again" }));
    const link = (await screen.findByRole("link", { name: /sign in/i })) as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe(URL_TWO);
    expect(calls.starts).toHaveLength(2);
    expect(codeInput().value).toBe("");

    submitCode("second-code");
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(calls.codes.map((c) => c.loginId)).toEqual(["login-1", "login-2"]);
  });

  it("treats an ended login (404 without a message) like a failed one", async () => {
    mockApi({ code: () => err(404, "login_not_found") });
    render(<SandboxLoginDialog onClose={() => {}} />);
    await ready();
    submitCode();
    expect(await screen.findByText(NOT_FOUND_TEXT)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Start again" })).toBeTruthy();
  });

  it("restores the code and asks to press Submit again when a submit needs elevation", async () => {
    let first = true;
    const calls = mockApi({
      code: () => {
        if (first) {
          first = false;
          return err(403, "elevation_required", "Re-enter your passphrase.");
        }
        return HttpResponse.json({ ok: true });
      },
    });
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    expect(await screen.findByText(ELEVATION_SUBMIT_TEXT)).toBeTruthy();
    expect(screen.queryByText(/Log in again/)).toBeNull();
    expect(screen.queryByRole("button", { name: /^Log in/ })).toBeNull();
    expect(codeInput().value).toBe(CODE);

    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(calls.codes.map((c) => c.loginId)).toEqual(["login-1", "login-1"]);
  });

  it("closes as logged in when a retry after a network error finds the login already done", async () => {
    mockApi({ code: lostFirstAnswer(() => err(404, "login_not_found")) });
    mockOutcome("succeeded");
    const onClose = vi.fn();
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(codeInput().value).toBe(CODE);
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("still ends the login on a 404 when no earlier submit failed on the network", async () => {
    mockApi({ code: () => err(404, "login_not_found") });
    const asked = mockOutcome("succeeded");
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();
    submitCode();
    expect(await screen.findByText(NOT_FOUND_TEXT)).toBeTruthy();
    expect(onLoggedIn).not.toHaveBeenCalled();
    expect(asked).toEqual([]);
  });

  it("shows a start error with the server message and a retry", async () => {
    const calls = mockApi({
      start: () => err(409, "sandbox_login_busy", "Another process runs a login for this credential."),
    });
    render(<SandboxLoginDialog onClose={() => {}} />);
    expect(await screen.findByText("Another process runs a login for this credential.")).toBeTruthy();
    expect(screen.queryByLabelText("Sign-in code")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(calls.starts).toHaveLength(2));
  });

  it("offers a forced login when the credential is already usable", async () => {
    const calls = mockApi({
      start: () => err(409, "sandbox_login_already_ok", "The credential is usable; send force to log in again."),
    });
    render(<SandboxLoginDialog onClose={() => {}} />);
    await screen.findByText(/credential is usable/);
    server.use(
      http.post("*/api/sandbox/login", async ({ request }) => {
        recordStart(calls, await request.json());
        return HttpResponse.json({ login_id: "login-9", url: URL_ONE });
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Log in again" }));
    await ready();
    expect(calls.starts).toEqual([{ force: false }, { force: true }]);
  });

  it("asks for the passphrase when starting needs elevation, then Log in works", async () => {
    let first = true;
    const calls = mockApi({
      start: () => {
        if (first) {
          first = false;
          return err(403, "elevation_required", "Re-enter your passphrase.");
        }
        return HttpResponse.json({ login_id: "login-1", url: URL_ONE });
      },
    });
    render(<SandboxLoginDialog onClose={() => {}} />);
    expect(await screen.findByText(/confirm your passphrase/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Log in" }));
    await ready();
    expect(calls.starts).toHaveLength(2);
  });

  // The server may have joined the dialog although the answer never came: the dialog keeps one
  // lease for every start it repeats, and leaves by it without the login id it never learned.
  it("retries a start whose answer was lost with the same lease", async () => {
    let answered = false;
    const calls = mockApi({
      start: () => {
        if (answered) return HttpResponse.json({ login_id: "login-1", lease: "lease-1", url: URL_ONE });
        answered = true;
        return HttpResponse.error();
      },
    });
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await screen.findByText("Network error.");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await ready();
    expect(calls.startLeases).toHaveLength(2);
    expect(calls.startLeases[0]).toMatch(/^[0-9a-f]{32}$/);
    expect(calls.startLeases[1]).toBe(calls.startLeases[0]);
  });

  it("leaves by its lease when it is closed after a start whose answer was lost", async () => {
    const calls = mockApi({ start: () => HttpResponse.error() });
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await screen.findByText("Network error.");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(calls.leaves).toEqual([calls.startLeases[0]]));
    expect(calls.cancels).toEqual([]);
  });

  it("leaves by its lease when it closed while a start's answer was lost", async () => {
    const hold = gate();
    const calls = mockApi({
      start: async () => {
        await hold.opened;
        return HttpResponse.error();
      },
    });
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} />);
    await waitFor(() => expect(calls.starts).toHaveLength(1));
    unmount();
    hold.open();
    await waitFor(() => expect(calls.leaves).toEqual([calls.startLeases[0]]));
  });

  it("leaves no lease behind a start the server refused for good", async () => {
    const calls = mockApi({ start: () => err(409, "sandbox_login_busy", "Another process runs a login.") });
    render(<SandboxLoginDialog onClose={() => {}} />);
    await screen.findByText("Another process runs a login.");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    // A leave is fire and forget: let a wrongly sent one reach the server before looking.
    await new Promise((resolve) => setTimeout(resolve, 0));
    await Promise.allSettled([...cancelsInFlight]);
    expect(calls.leaves).toEqual([]);
    expect(calls.cancels).toEqual([]);
  });

  it("still leaves by its lease after a retry the server refused, when an earlier answer was lost", async () => {
    let first = true;
    const calls = mockApi({
      start: () => {
        if (first) {
          first = false;
          return HttpResponse.error();
        }
        return err(403, "elevation_required", "Re-enter your passphrase.");
      },
    });
    render(<SandboxLoginDialog onClose={() => {}} />);
    await screen.findByText("Network error.");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByText(/confirm your passphrase/i);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(calls.leaves).toEqual([calls.startLeases[0]]));
  });

  it("cancels the running login and discards the code on Cancel", async () => {
    const calls = mockApi({});
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await ready();
    fireEvent.change(codeInput(), { target: { value: CODE } });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
    // Only this dialog's part of the login: another dialog may share it.
    expect(calls.leases).toEqual(["lease-1"]);
    expect(calls.codes).toEqual([]);
  });

  it("yields Tab and Escape to a modal opened above it, such as the passphrase prompt", async () => {
    const calls = mockApi({});
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await ready();
    const prompt = document.createElement("div");
    prompt.setAttribute("role", "dialog");
    const passphrase = document.createElement("input");
    prompt.appendChild(passphrase);
    document.body.appendChild(prompt);
    try {
      passphrase.focus();
      fireEvent.keyDown(passphrase, { key: "Tab" });
      expect(document.activeElement).toBe(passphrase);
      fireEvent.keyDown(passphrase, { key: "Escape" });
      expect(onClose).not.toHaveBeenCalled();
      expect(calls.cancels).toEqual([]);
    } finally {
      prompt.remove();
    }
  });

  it("keeps the same login usable when the code is refused as too large", async () => {
    let first = true;
    const calls = mockApi({
      code: () => {
        if (first) {
          first = false;
          return err(413, "payload_too_large");
        }
        return HttpResponse.json({ ok: true });
      },
    });
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode("x".repeat(5000));
    expect((await screen.findByRole("alert")).textContent).toMatch(/too long/i);
    expect(codeInput().value).toBe("x".repeat(5000));
    expect(screen.queryByRole("button", { name: "Start again" })).toBeNull();

    submitCode("good-code");
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(calls.codes.map((c) => c.loginId)).toEqual(["login-1", "login-1"]);
    expect(calls.starts).toHaveLength(1);
  });

  it.each([
    ["the lookup itself fails", ["error", "succeeded"]],
    ["the exchange is still pending", ["pending", "succeeded"]],
  ])("keeps the login while the outcome is unsettled: %s", async (_name, outcomes) => {
    mockApi({
      code: lostFirstAnswer(
        () => err(404, "login_not_found"),
        () => err(404, "login_not_found"),
      ),
    });
    const asked = mockOutcome(...outcomes);
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect((await screen.findByRole("alert")).textContent).toMatch(/could not confirm/i);
    expect(screen.queryByText(NOT_FOUND_TEXT)).toBeNull();
    expect(onLoggedIn).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
    expect(asked).toEqual(["login-1", "login-1"]);
  });

  it.each(["failed", "unknown"])(
    "ends the login after a lost answer when the exchange of that login came to %s, whatever credential exists",
    async (outcome) => {
      mockApi({ code: lostFirstAnswer(() => err(404, "login_not_found")) });
      mockOutcome(outcome);
      // A usable credential from before the login proves nothing about this exchange.
      server.use(
        http.get("*/api/sandbox/login", () =>
          HttpResponse.json({ status: { state: "ok" }, running: null, held_sessions: 0 }),
        ),
      );
      const onLoggedIn = vi.fn();
      render(<SandboxLoginDialog force onClose={() => {}} onLoggedIn={onLoggedIn} />);
      await ready();

      submitCode();
      expect(await screen.findByRole("alert")).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Submit" }));
      expect(await screen.findByText(NOT_FOUND_TEXT)).toBeTruthy();
      expect(onLoggedIn).not.toHaveBeenCalled();
    },
  );

  it("keeps a lost answer in mind through a refused code, not only an elevation request", async () => {
    mockApi({
      code: lostFirstAnswer(
        () => err(400, "invalid_code", "The code has a character it cannot have."),
        () => err(404, "login_not_found"),
      ),
    });
    mockOutcome("succeeded");
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(await screen.findByText("The code has a character it cannot have.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
  });

  it("keeps a lost answer in mind through an elevation request", async () => {
    let n = 0;
    mockApi({
      code: () => {
        n += 1;
        if (n === 1) return HttpResponse.error();
        return n === 2 ? err(403, "elevation_required", "Re-enter your passphrase.") : err(404, "login_not_found");
      },
    });
    mockOutcome("succeeded");
    const onLoggedIn = vi.fn();
    render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();

    submitCode();
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(await screen.findByText(ELEVATION_SUBMIT_TEXT)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(onLoggedIn).toHaveBeenCalledTimes(1));
  });

  it("stays usable when rendered inside a disabled fieldset, as Settings does while offline", async () => {
    mockApi({});
    render(
      <fieldset disabled>
        <SandboxLoginDialog onClose={() => {}} />
      </fieldset>,
    );
    await ready();
    expect(screen.getByRole("button", { name: "Cancel" }).matches(":disabled")).toBe(false);
    expect(codeInput().matches(":disabled")).toBe(false);
  });

  it("moves focus into the dialog while the login is still starting", async () => {
    const hold = gate();
    mockApi({
      start: async () => {
        await hold.opened;
        return HttpResponse.json({ login_id: "login-1", url: URL_ONE });
      },
    });
    const opener = document.createElement("button");
    document.body.appendChild(opener);
    opener.focus();
    try {
      render(<SandboxLoginDialog onClose={() => {}} />);
      await screen.findByText(/Starting the login/);
      expect(screen.getByRole("dialog").contains(document.activeElement)).toBe(true);
      hold.open();
      await ready();
    } finally {
      opener.remove();
    }
  });

  it("closes on Escape like Cancel", async () => {
    const calls = mockApi({});
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await ready();
    fireEvent.keyDown(codeInput(), { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
  });

  it("ignores Cancel, the backdrop and Escape while the code is being submitted", async () => {
    const hold = gate();
    const calls = mockApi({
      code: async () => {
        await hold.opened;
        return HttpResponse.json({ ok: true });
      },
    });
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await ready();
    submitCode();
    await screen.findByRole("button", { name: /submitting/i });

    expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("dialog"));
    fireEvent.keyDown(document.body, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
    expect(calls.cancels).toEqual([]);

    hold.open();
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
    expect(calls.cancels).toEqual([]);
  });

  it("cancels a login that starts after the dialog was closed", async () => {
    const hold = gate();
    const calls = mockApi({
      start: async () => {
        await hold.opened;
        return HttpResponse.json({ login_id: "login-1", url: URL_ONE });
      },
    });
    const onClose = vi.fn();
    render(<SandboxLoginDialog onClose={onClose} />);
    await waitFor(() => expect(calls.starts).toHaveLength(1));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(1);
    hold.open();
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("cancels a login that starts after the dialog unmounted", async () => {
    const hold = gate();
    const calls = mockApi({
      start: async () => {
        await hold.opened;
        return HttpResponse.json({ login_id: "login-1", url: URL_ONE });
      },
    });
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} />);
    await waitFor(() => expect(calls.starts).toHaveLength(1));
    unmount();
    hold.open();
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
  });

  it("leaves the running login with its own lease when the dialog unmounts", async () => {
    const calls = mockApi({});
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} />);
    await ready();
    unmount();
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
    expect(calls.leases).toEqual(["lease-1"]);
    expect(calls.codes).toEqual([]);
  });

  it("lets a pending code exchange finish when the dialog unmounts, since a cancel would kill it", async () => {
    const hold = gate();
    const calls = mockApi({
      code: async () => {
        await hold.opened;
        return HttpResponse.json({ ok: true });
      },
    });
    const onLoggedIn = vi.fn();
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} onLoggedIn={onLoggedIn} />);
    await ready();
    submitCode();
    await waitFor(() => expect(calls.codes).toHaveLength(1));
    unmount();
    hold.open();
    // The server settles an abandoned exchange itself, so the client has nothing to cancel.
    // A request sent after the unmount reaches the server after any cancel the unmount sent.
    server.use(http.get("*/sentinel", () => new HttpResponse(null, { status: 204 })));
    await fetch("/sentinel");
    expect(calls.cancels).toEqual([]);
    expect(onLoggedIn).not.toHaveBeenCalled();
  });

  it("cancels the login a refused code left running once the unmounted dialog hears the refusal", async () => {
    const hold = gate();
    const calls = mockApi({
      code: async () => {
        await hold.opened;
        return err(400, "invalid_code", "That code is not valid.");
      },
    });
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} />);
    await ready();
    submitCode();
    await waitFor(() => expect(calls.codes).toHaveLength(1));
    unmount();
    expect(calls.cancels).toEqual([]);
    hold.open();
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
  });

  it("releases its lease once the unmounted dialog's code request failed before reaching the server", async () => {
    const hold = gate();
    const calls = mockApi({
      code: async () => {
        await hold.opened;
        return HttpResponse.error();
      },
    });
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} />);
    await ready();
    submitCode();
    await waitFor(() => expect(calls.codes).toHaveLength(1));
    unmount();
    expect(calls.cancels).toEqual([]);
    hold.open();
    await waitFor(() => expect(calls.leases).toEqual(["lease-1"]));
  });

  it("neither cancels a live login nor leaks one under StrictMode's double mount", async () => {
    const calls = mockApi({});
    const { unmount } = render(
      <StrictMode>
        <SandboxLoginDialog onClose={() => {}} />
      </StrictMode>,
    );
    await ready();
    await act(async () => {});
    expect(calls.starts).toHaveLength(1);
    expect(calls.cancels).toEqual([]);
    expect(codeInput()).toBeTruthy();
    unmount();
    await waitFor(() => expect(calls.cancels).toEqual(["login-1"]));
  });

  it("keeps the code out of storage, the URL, history, the console and every other request", async () => {
    const consoleSpies = (["log", "info", "warn", "error", "debug", "trace"] as const).map((m) =>
      vi.spyOn(console, m).mockImplementation(() => {}),
    );
    const storageWrites: string[] = [];
    const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation((key, value) => {
      storageWrites.push(`${key}=${value}`);
    });
    const historyWrites: unknown[] = [];
    const push = vi.spyOn(window.history, "pushState").mockImplementation((...a) => void historyWrites.push(a));
    const replace = vi.spyOn(window.history, "replaceState").mockImplementation((...a) => void historyWrites.push(a));
    const requests: { method: string; url: string; body: string }[] = [];
    server.events.on("request:start", async ({ request }) => {
      requests.push({ method: request.method, url: request.url, body: await request.clone().text() });
    });
    let first = true;
    mockApi({
      code: () => {
        if (first) {
          first = false;
          return err(400, "invalid_code", "The code is malformed.");
        }
        return err(422, "login_failed", "Login failed: Invalid code.");
      },
    });
    server.use(http.post("*/api/client-log", () => new HttpResponse(null, { status: 204 })));
    const before = window.location.href;

    const { container } = render(<SandboxLoginDialog onClose={() => {}} />);
    await ready();
    submitCode();
    await screen.findByText("The code is malformed.");
    submitCode();
    await screen.findByText("Login failed: Invalid code.");
    expect(window.location.href).toBe(before);
    expect(container.ownerDocument.body.innerHTML).not.toContain(CODE);

    for (const spy of consoleSpies) {
      for (const call of spy.mock.calls) expect(JSON.stringify(call)).not.toContain(CODE);
    }
    expect(storageWrites.join("\n")).not.toContain(CODE);
    expect(JSON.stringify(historyWrites)).not.toContain(CODE);
    expect(window.localStorage.getItem("sandbox-login-code")).toBeNull();
    for (const request of requests) {
      expect(request.url).not.toContain(CODE);
      if (!request.url.endsWith("/code")) expect(request.body).not.toContain(CODE);
      expect(request.url).not.toContain("client-log");
    }
    expect(requests.filter((r) => r.body.includes(CODE)).map((r) => r.method)).toEqual(["POST", "POST"]);

    server.events.removeAllListeners();
    for (const spy of consoleSpies) spy.mockRestore();
    setItem.mockRestore();
    push.mockRestore();
    replace.mockRestore();
  });
});

describe("SandboxLoginStatusRow", () => {
  const status = (state: string, onRead?: () => void) =>
    server.use(
      http.get("*/api/sandbox/login", () => {
        onRead?.();
        return HttpResponse.json({ status: { state }, running: null, held_sessions: 0 });
      }),
    );

  it("removes the row once the setting no longer applies", async () => {
    status("ok");
    const { container, rerender } = render(<SandboxLoginStatusRow reloadKey={0} />);
    await screen.findByText("Sandbox login: ok");
    status("not_applicable");
    rerender(<SandboxLoginStatusRow reloadKey={1} />);
    await waitFor(() => expect(screen.queryByText("Sandbox login: ok")).toBeNull());
    expect(container.firstChild).toBeNull();
  });

  it("shows plain text and a Log in button for a login that is needed", async () => {
    status("needed");
    mockApi({});
    render(<SandboxLoginStatusRow />);
    expect(await screen.findByText("Sandbox login: needed")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Log in" }));
    await ready();
    submitCode();
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("shows ok with a Log in again button that starts a forced login and refreshes on success", async () => {
    status("ok");
    const calls = mockApi({});
    render(<SandboxLoginStatusRow />);
    expect(await screen.findByText("Sandbox login: ok")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Log in" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Log in again" }));
    await ready();
    expect(calls.starts).toEqual([{ force: true }]);
  });

  it("reads and logs in the profile on screen, and re-reads when the reload key changes", async () => {
    const asked: (string | null)[] = [];
    server.use(
      http.get("*/api/sandbox/login", ({ request }) => {
        asked.push(new URL(request.url).searchParams.get("profile"));
        return HttpResponse.json({ status: { state: "needed" }, running: null, held_sessions: 0 });
      }),
    );
    const calls = mockApi({});
    const { rerender } = render(<SandboxLoginStatusRow profile="work" reloadKey={0} />);
    await screen.findByText("Sandbox login: needed");
    expect(asked).toEqual(["work"]);

    rerender(<SandboxLoginStatusRow profile="work" reloadKey={1} />);
    await waitFor(() => expect(asked).toEqual(["work", "work"]));

    fireEvent.click(screen.getByRole("button", { name: "Log in" }));
    await ready();
    expect(calls.starts).toEqual([{ force: false, profile: "work" }]);
  });

  it("hides the previous profile's status until the new profile's status has loaded", async () => {
    const hold = gate();
    server.use(
      http.get("*/api/sandbox/login", async ({ request }) => {
        const profile = new URL(request.url).searchParams.get("profile");
        if (profile === "other") await hold.opened;
        return HttpResponse.json({
          status: { state: profile === "other" ? "needed" : "ok" },
          running: null,
          held_sessions: 0,
        });
      }),
    );
    const { rerender } = render(<SandboxLoginStatusRow profile="work" />);
    await screen.findByText("Sandbox login: ok");
    rerender(<SandboxLoginStatusRow profile="other" />);
    expect(screen.queryByText("Sandbox login: ok")).toBeNull();
    expect(screen.queryByRole("button")).toBeNull();
    hold.open();
    expect(await screen.findByText("Sandbox login: needed")).toBeTruthy();
  });

  it("closes its dialog with a profile change and does not reopen it when the new status arrives", async () => {
    const hold = gate();
    server.use(
      http.get("*/api/sandbox/login", async ({ request }) => {
        if (new URL(request.url).searchParams.get("profile") === "other") await hold.opened;
        return HttpResponse.json({ status: { state: "needed" }, running: null, held_sessions: 0 });
      }),
    );
    const calls = mockApi({});
    const { rerender } = render(<SandboxLoginStatusRow profile="work" />);
    await screen.findByText("Sandbox login: needed");
    fireEvent.click(screen.getByRole("button", { name: "Log in" }));
    await ready();

    rerender(<SandboxLoginStatusRow profile="other" />);
    hold.open();
    expect(await screen.findByText("Sandbox login: needed")).toBeTruthy();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(calls.starts).toEqual([{ force: false, profile: "work" }]);
  });

  it("shows the status but no Log in button on a read-only server", async () => {
    status("needed");
    render(<SandboxLoginStatusRow readOnly />);
    expect(await screen.findByText("Sandbox login: needed")).toBeTruthy();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("re-reads the status when the dialog closes, after success or Cancel", async () => {
    let reads = 0;
    status("needed", () => {
      reads += 1;
    });
    mockApi({});
    render(<SandboxLoginStatusRow />);
    await screen.findByText("Sandbox login: needed");
    expect(reads).toBe(1);

    fireEvent.click(screen.getByRole("button", { name: "Log in" }));
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(reads).toBe(2));

    status("ok", () => {
      reads += 1;
    });
    fireEvent.click(screen.getByRole("button", { name: "Log in" }));
    await ready();
    submitCode();
    expect(await screen.findByText("Sandbox login: ok")).toBeTruthy();
    expect(reads).toBe(3);
  });
});

describe("SandboxLoginDialog focus", () => {
  it("keeps Tab inside the dialog and returns focus to the opener when it closes", async () => {
    mockApi({});
    const opener = document.createElement("button");
    document.body.appendChild(opener);
    opener.focus();
    const { unmount } = render(<SandboxLoginDialog onClose={() => {}} />);
    await ready();
    const dialog = screen.getByRole("dialog");
    const items = dialog.querySelectorAll<HTMLElement>("button:not([disabled]), a[href], input");
    const last = items[items.length - 1];
    last.focus();
    fireEvent.keyDown(document, { key: "Tab" });
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(document, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(last);

    opener.focus();
    fireEvent.keyDown(document, { key: "Tab" });
    expect(dialog.contains(document.activeElement)).toBe(true);

    unmount();
    expect(document.activeElement).toBe(opener);
    opener.remove();
  });
});
