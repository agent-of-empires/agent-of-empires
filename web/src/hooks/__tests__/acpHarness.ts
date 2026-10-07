// Shared fakes for useAcpSession hook tests: a scriptable WebSocket and a recording fetch router.

import { act } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { vi } from "vitest";
import { AgentProfileProvider } from "../../lib/agentProfileContext";

export class FakeWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  /** When true, `close()` fires `onclose` like a real socket. */
  static closeFiresOnClose = false;
  static sockets: FakeWebSocket[] = [];

  readyState = FakeWebSocket.CONNECTING;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;

  url: string;
  protocols?: string | string[];

  constructor(url: string, protocols?: string | string[]) {
    this.url = url;
    this.protocols = protocols;
    FakeWebSocket.sockets.push(this);
  }

  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    if (FakeWebSocket.closeFiresOnClose) this.onclose?.({ code: 1000, reason: "test", wasClean: true } as CloseEvent);
  }

  send(): void {}

  open(): void {
    act(() => {
      this.readyState = FakeWebSocket.OPEN;
      this.onopen?.(new Event("open"));
    });
  }

  drop(): void {
    act(() => {
      this.readyState = FakeWebSocket.CLOSED;
      this.onclose?.({ code: 1006, reason: "", wasClean: false } as CloseEvent);
    });
  }

  message(data: unknown): void {
    act(() => {
      this.onmessage?.({ data: JSON.stringify(data) } as MessageEvent);
    });
  }
}

export const sockets = FakeWebSocket.sockets;
export const lastSocket = (): FakeWebSocket => sockets[sockets.length - 1]!;

export interface Call {
  method: string;
  url: string;
  body: string | null;
}

export type Route = (call: Call) => Response | Promise<Response> | undefined;

export const json = (body: unknown, status = 200): Response => new Response(JSON.stringify(body), { status });
export const emptyReplay = (): Response => json({ frames: [], lost: false, highest_seq: 0 });

/** Install the fake socket and a fetch that records calls, answers `route`, and falls back to an empty replay. */
export function installAcpFakes(route: Route = () => undefined): Call[] {
  const calls: Call[] = [];
  sockets.length = 0;
  FakeWebSocket.closeFiresOnClose = false;
  vi.stubGlobal("WebSocket", FakeWebSocket);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const call = {
        method: (init?.method ?? "GET").toUpperCase(),
        url: typeof input === "string" ? input : input.toString(),
        body: typeof init?.body === "string" ? init.body : null,
      };
      calls.push(call);
      const res = await route(call);
      if (res) return res;
      return call.url.includes("/acp/replay") ? emptyReplay() : json({});
    }),
  );
  return calls;
}

export async function flushAsync(ticks = 10): Promise<void> {
  await act(async () => {
    for (let i = 0; i < ticks; i++) await Promise.resolve();
  });
}

export const profileWrapper = ({ children }: { children: ReactNode }) =>
  createElement(AgentProfileProvider, { toolKey: "claude", children });
