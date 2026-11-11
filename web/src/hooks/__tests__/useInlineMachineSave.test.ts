// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, expect, it } from "vitest";
import { INLINE_ELEVATION_HEADER } from "../../lib/fetchInterceptor";
import { useInlineMachineSave } from "../useInlineMachineSave";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

it("asks for the passphrase inline, then retries the same write once", async () => {
  const bodies: unknown[] = [];
  const headers: (string | null)[] = [];
  let elevated = false;
  server.use(
    http.patch("*/api/settings", async ({ request }) => {
      bodies.push(await request.json());
      headers.push(request.headers.get(INLINE_ELEVATION_HEADER));
      return elevated
        ? new HttpResponse(null, { status: 204 })
        : HttpResponse.json({ error: "elevation_required", message: "Re-enter" }, { status: 403 });
    }),
    http.post("*/api/login/elevate", async ({ request }) => {
      const { passphrase } = (await request.json()) as { passphrase: string };
      if (passphrase !== "right")
        return HttpResponse.json({ message: "That passphrase did not match." }, { status: 401 });
      elevated = true;
      return HttpResponse.json({ elevated_until_secs: 900 });
    }),
  );
  const { result } = renderHook(() => useInlineMachineSave());
  let saved: Promise<boolean> | undefined;
  act(() => {
    saved = result.current.save({ worktree: { enabled: true } });
  });
  await waitFor(() => expect(result.current.elevation).not.toBeNull());

  let error: string | null = null;
  await act(async () => {
    error = await result.current.elevation!.submit("wrong");
  });
  expect(error).toBe("That passphrase did not match.");
  expect(result.current.elevation).not.toBeNull();

  await act(async () => {
    error = await result.current.elevation!.submit("right");
  });
  expect(error).toBeNull();
  await expect(saved).resolves.toBe(true);
  expect(result.current.elevation).toBeNull();
  expect(bodies).toEqual([{ worktree: { enabled: true } }, { worktree: { enabled: true } }]);
  expect(headers).toEqual(["1", "1"]);
});

it("a newer write cancels the pending prompt, and cancel resolves as not saved", async () => {
  server.use(http.patch("*/api/settings", () => HttpResponse.json({ error: "elevation_required" }, { status: 403 })));
  const { result } = renderHook(() => useInlineMachineSave());
  let first: Promise<boolean> | undefined;
  act(() => {
    first = result.current.save({ sandbox: { enabled_by_default: true } });
  });
  await waitFor(() => expect(result.current.elevation).not.toBeNull());
  const firstPrompt = result.current.elevation;
  let second: Promise<boolean> | undefined;
  act(() => {
    second = result.current.save({ sandbox: { default_image: "x" } });
  });
  await expect(first).resolves.toBe(false);
  await waitFor(() => expect(result.current.elevation).not.toBeNull());
  expect(result.current.elevation).not.toBe(firstPrompt);
  act(() => result.current.elevation!.cancel());
  await expect(second).resolves.toBe(false);
  expect(result.current.elevation).toBeNull();
});

it("a save that may not have landed is not reported as saved", async () => {
  server.use(http.patch("*/api/settings", () => new HttpResponse(null, { status: 502 })));
  const { result } = renderHook(() => useInlineMachineSave());
  await expect(result.current.save({ worktree: { enabled: true } })).resolves.toBe(false);
});

it("a request cancelled while elevating never retries its write", async () => {
  const bodies: unknown[] = [];
  let elevate!: () => void;
  server.use(
    http.patch("*/api/settings", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json({ error: "elevation_required" }, { status: 403 });
    }),
    http.post(
      "*/api/login/elevate",
      () =>
        new Promise<Response>((resolve) => (elevate = () => resolve(HttpResponse.json({ elevated_until_secs: 900 })))),
    ),
  );
  const { result } = renderHook(() => useInlineMachineSave());
  let first: Promise<boolean> | undefined;
  act(() => {
    first = result.current.save({ worktree: { enabled: true } });
  });
  await waitFor(() => expect(result.current.elevation).not.toBeNull());
  let submitted: Promise<string | null> | undefined;
  act(() => {
    submitted = result.current.elevation!.submit("right");
  });
  await waitFor(() => expect(elevate).toBeDefined());
  act(() => {
    void result.current.save({ sandbox: { enabled_by_default: true } });
  });
  await expect(first).resolves.toBe(false);
  await act(async () => {
    elevate();
    await submitted;
  });
  expect(bodies).toEqual([{ worktree: { enabled: true } }, { sandbox: { enabled_by_default: true } }]);
});

it("an older write's late 403 doesn't replace the newer write's prompt", async () => {
  const committed: unknown[] = [];
  let releaseOlder: (() => void) | undefined;
  let elevated = false;
  server.use(
    http.patch("*/api/settings", async ({ request }) => {
      const body = (await request.json()) as { sandbox: { container_runtime: string } };
      if (elevated) {
        committed.push(body);
        return new HttpResponse(null, { status: 204 });
      }
      if (body.sandbox.container_runtime === "docker") await new Promise<void>((r) => (releaseOlder = r));
      return HttpResponse.json({ error: "elevation_required" }, { status: 403 });
    }),
    http.post("*/api/login/elevate", () => {
      elevated = true;
      return HttpResponse.json({ elevated_until_secs: 900 });
    }),
  );
  const { result } = renderHook(() => useInlineMachineSave());
  let older: Promise<boolean> | undefined;
  let newer: Promise<boolean> | undefined;
  act(() => {
    older = result.current.save({ sandbox: { container_runtime: "docker" } });
  });
  await waitFor(() => expect(releaseOlder).toBeDefined());
  act(() => {
    newer = result.current.save({ sandbox: { container_runtime: "podman" } });
  });
  releaseOlder!();
  expect(await older).toBe(false);
  await waitFor(() => expect(result.current.elevation).not.toBeNull());
  const prompt = result.current.elevation;

  await act(async () => void (await prompt!.submit("right")));
  expect(await newer).toBe(true);
  expect(committed).toEqual([{ sandbox: { container_runtime: "podman" } }]);
});

it("a newer committed save retires an older retry still waiting on elevation", async () => {
  const committed: unknown[] = [];
  let elevated = false;
  let elevate: (() => void) | undefined;
  server.use(
    http.patch("*/api/settings", async ({ request }) => {
      const body = await request.json();
      if (!elevated) return HttpResponse.json({ error: "elevation_required" }, { status: 403 });
      committed.push(body);
      return new HttpResponse(null, { status: 204 });
    }),
    // The server grants elevation, but its reply reaches the browser late.
    http.post("*/api/login/elevate", async () => {
      elevated = true;
      await new Promise<void>((r) => (elevate = r));
      return HttpResponse.json({ elevated_until_secs: 900 });
    }),
  );
  const { result } = renderHook(() => useInlineMachineSave());
  let older: Promise<boolean> | undefined;
  act(() => {
    older = result.current.save({ worktree: { enabled: true } });
  });
  await waitFor(() => expect(result.current.elevation).not.toBeNull());
  let submitted: Promise<string | null> | undefined;
  act(() => {
    submitted = result.current.elevation!.submit("right");
  });
  await waitFor(() => expect(elevate).toBeDefined());

  let newer: Promise<boolean> | undefined;
  act(() => {
    newer = result.current.save({ worktree: { enabled: false } });
  });
  expect(await newer).toBe(true);

  await act(async () => {
    elevate!();
    await submitted;
  });
  expect(committed).toEqual([{ worktree: { enabled: false } }]);
  expect(await older).toBe(false);
});
