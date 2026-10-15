import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  fetchPluginUiState,
  fetchSessions,
  invokePluginAction,
  mintPluginPageTicket,
  type PluginPage,
} from "../../lib/api";
import {
  BRIDGE_VERSION,
  bridgeError,
  bridgePush,
  bridgeReply,
  parseRequest,
  resolveRequest,
  themePayload,
  type BridgeCall,
} from "../../lib/pluginBridge";
import { usePluginPages } from "../../lib/pluginUiContext";
import { requestOpenSession } from "../../lib/sessionRoute";
import { THEME_CHANGED_EVENT, type ResolvedTheme } from "../../lib/theme";
import { listen } from "../../hooks/domEvents";

interface Props {
  pluginId: string;
  pageId: string;
  onBack: () => void;
  readOnly: boolean;
  cityhall: boolean;
  theme: ResolvedTheme | null;
}

function closePort(port: { current: MessagePort | null }) {
  port.current?.close();
  port.current = null;
}

function newNonce(): string {
  // getRandomValues, unlike randomUUID, also works on a plain-HTTP dashboard.
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
}

async function runCall(call: BridgeCall, pluginId: string): Promise<unknown> {
  switch (call.method) {
    case "ready":
      return null;
    case "sessions.list": {
      const reply = await fetchSessions();
      if (!reply) throw new Error("sessions unavailable");
      return reply.sessions;
    }
    case "plugins.uiState": {
      const reply = await fetchPluginUiState();
      if (!reply) throw new Error("ui state unavailable");
      return reply;
    }
    case "action": {
      // Always the frame's own plugin: a page can never reach another plugin's worker.
      const reply = await invokePluginAction(pluginId, call.action, call.sessionId, call.params);
      if (!reply) throw new Error("action refused");
      return reply;
    }
    case "openSession":
      requestOpenSession(call.sessionId);
      return null;
  }
}

/** A plugin page, full width. The frame gets an opaque origin: no dashboard storage, cookies, token or API; the bridge is its only data path. */
export function PluginPageView({ pluginId, pageId, onBack, readOnly, cityhall, theme }: Props) {
  const pages = usePluginPages();
  const page = pages?.find((p) => p.plugin_id === pluginId && p.id === pageId);
  const [attempt, setAttempt] = useState(0);
  const [minted, setMinted] = useState<{ page: PluginPage; attempt: number; ok: boolean; nonce: string } | null>(null);
  const [lost, setLost] = useState(false);
  const frameRef = useRef<HTMLIFrameElement | null>(null);
  // A frame's second load is a new document behind the same window, so it no longer holds the handshake.
  const loads = useRef(0);
  // Transferred by the page with `ready`; a later document can never reach it, so nothing crosses a navigation.
  const port = useRef<MessagePort | null>(null);
  // Rides the frame URL's fragment, which never reaches the server; `ready` must echo it, so a document the page navigates to cannot take its identity.
  const nonce = useRef<string | null>(null);

  const attachFrame = useCallback((frame: HTMLIFrameElement | null) => {
    frameRef.current = frame;
    loads.current = 0;
    closePort(port);
  }, []);

  // The frame load authenticates with the ticket this sets, so mint before rendering it.
  useEffect(() => {
    if (!page) return;
    let cancelled = false;
    void mintPluginPageTicket(page).then((ok) => {
      if (cancelled) return;
      nonce.current = newNonce();
      setMinted({ page, attempt, ok, nonce: nonce.current });
    });
    return () => {
      cancelled = true;
    };
  }, [page, attempt]);

  // Installed before the frame exists, so a page's handshake cannot beat it.
  useEffect(() => {
    if (!page) return;
    const grants = { capabilities: page.capabilities, readOnly, cityhall };
    const serve = (channel: MessagePort) => (e: MessageEvent) => {
      const call = parseRequest(e.data);
      if (!call) return;
      const resolved = resolveRequest(call, grants);
      if ("error" in resolved) {
        channel.postMessage(bridgeError(call.id, resolved.error, resolved.message));
        return;
      }
      runCall(resolved.call, page.plugin_id).then(
        (result) => channel.postMessage(bridgeReply(call.id, result)),
        () => channel.postMessage(bridgeError(call.id, "failed", `${call.method} failed.`)),
      );
    };
    // An open channel resolves against the current modes, not those at its handshake.
    if (port.current) port.current.onmessage = serve(port.current);
    const onMessage = (event: MessageEvent) => {
      // `event.origin` is "null" for the sandboxed frame; only the window identifies it.
      const frame = frameRef.current?.contentWindow;
      if (!frame || event.source !== frame || loads.current > 1 || port.current) return;
      const req = parseRequest(event.data);
      const channel = event.ports[0];
      if (req?.method !== "ready" || !channel || !nonce.current || req.params.nonce !== nonce.current) return;
      port.current = channel;
      channel.onmessage = serve(channel);
      channel.postMessage(bridgeReply(req.id, null));
      channel.postMessage(
        bridgePush("context", { readOnly, cityhall, pluginId, pageId, bridgeVersion: BRIDGE_VERSION }),
      );
      if (theme) channel.postMessage(bridgePush("theme", themePayload(theme)));
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [page, pluginId, pageId, readOnly, cityhall, theme]);

  useEffect(
    () =>
      listen(
        (event) =>
          port.current?.postMessage(bridgePush("theme", themePayload((event as CustomEvent<ResolvedTheme>).detail))),
        [window, THEME_CHANGED_EVENT],
      ),
    [],
  );

  const onFrameLoad = () => {
    loads.current += 1;
    if (loads.current > 1) {
      closePort(port);
      setLost(true);
    }
  };

  const reopen = () => {
    setLost(false);
    setAttempt((a) => a + 1);
  };

  const ticket = page && minted?.page === page && minted.attempt === attempt ? minted : null;
  let body = null;
  if (pages !== null && !page) {
    body = <Empty title="Page not found" detail={`No active plugin ${pluginId} declares a page ${pageId}.`} />;
  } else if (page && ticket?.ok === false) {
    body = <Empty title="This page could not be opened" detail="Reload the dashboard to try again." />;
  } else if (lost) {
    body = (
      <Empty title="This page reloaded itself" detail="Its connection to the dashboard was closed.">
        <button onClick={reopen} className="mt-3 text-brand-500 cursor-pointer text-sm">
          Reopen
        </button>
      </Empty>
    );
  } else if (page && ticket?.ok) {
    body = (
      <iframe
        key={attempt}
        ref={attachFrame}
        title={page.title}
        src={`${page.url}#${ticket.nonce}`}
        sandbox="allow-scripts"
        onLoad={onFrameLoad}
        className="flex-1 w-full border-0 bg-surface-900"
        data-testid="plugin-page-frame"
      />
    );
  }

  return (
    <div className="flex-1 flex flex-col overflow-hidden bg-surface-900" data-testid="plugin-page-view">
      <div className="bg-surface-850 border-b border-surface-700 shrink-0 flex items-center gap-3 px-4 h-12">
        <button onClick={onBack} className="text-brand-500 cursor-pointer text-sm shrink-0">
          &larr; Back
        </button>
        <span className="text-xs font-mono text-text-bright truncate">{page?.title ?? pageId}</span>
      </div>
      {body}
    </div>
  );
}

function Empty({ title, detail, children }: { title: string; detail: string; children?: ReactNode }) {
  return (
    <div className="px-4 py-10 text-center" data-testid="plugin-page-empty">
      <p className="text-sm font-medium text-text-secondary">{title}</p>
      <p className="mt-1 text-[13px] text-text-muted">{detail}</p>
      {children}
    </div>
  );
}
