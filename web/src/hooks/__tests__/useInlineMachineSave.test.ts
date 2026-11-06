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

it("cancel resolves the save as not saved", async () => {
  server.use(http.patch("*/api/settings", () => HttpResponse.json({ error: "elevation_required" }, { status: 403 })));
  const { result } = renderHook(() => useInlineMachineSave());
  let saved: Promise<boolean> | undefined;
  act(() => {
    saved = result.current.save({ sandbox: { enabled_by_default: true } });
  });
  await waitFor(() => expect(result.current.elevation).not.toBeNull());
  act(() => result.current.elevation!.cancel());
  await expect(saved).resolves.toBe(false);
});
