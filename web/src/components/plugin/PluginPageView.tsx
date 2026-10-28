import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { ExternalLink, Maximize2, Minimize2, PanelBottom, X } from "lucide-react";
import {
  fetchPluginUiState,
  fetchPluginSessions,
  invokePluginAction,
  mintPluginPageTicket,
  setSessionArchive,
  trashSession,
} from "../../lib/api";
import {
  BRIDGE_VERSION,
  BridgeRefusal,
  bridgeError,
  bridgePush,
  bridgeReply,
  parseRequest,
  resolveRequest,
  themePayload,
  type BridgeCall,
} from "../../lib/pluginBridge";
import { isDeepLinkValue } from "../../lib/pluginHref";
import { usePluginPages } from "../../lib/pluginUiContext";
import type { SessionMenuRequest } from "./PluginSessionMenu";
import { requestOpenSession } from "../../lib/sessionRoute";
import { safeGetItem, safeSetItem } from "../../lib/safeStorage";
import { THEME_CHANGED_EVENT, type ResolvedTheme } from "../../lib/theme";
import { listen } from "../../hooks/domEvents";
import { useIsWideViewport } from "../../hooks/useIsWideViewport";
import { usePersistedFlag } from "../usePersistedFlag";

interface Props {
  pluginId: string;
  pageId: string;
  onBack: () => void;
  readOnly: boolean;
  cityhall: boolean;
  theme: ResolvedTheme | null;
  /** The `?link=` value of the dashboard URL; forwarded only to a page that declares `deep_link`. */
  link: string | null;
  /** The split pane's session, kept by the dashboard so its docks follow it. */
  selectedSessionId: string | null;
  onSelectSession: (id: string | null) => void;
  /** The selected session's title and full view, or null when the dashboard does not know it. */
  renderSession: () => { title: string; view: ReactNode } | null;
  /** The host session menu for `session.contextMenu`. */
  renderSessionMenu: (request: SessionMenuRequest) => ReactNode;
}

type MenuArgs = Omit<SessionMenuRequest, "key" | "onClose">;

interface PageHost {
  openSession: (id: string) => void;
  /** Resolves with the picked page item's id, or null. */
  openMenu: (request: Omit<MenuArgs, "source">) => Promise<string | null>;
}

const SPLIT_HEIGHT_KEY = "aoe-plugin-page-split-height";
const DEFAULT_SPLIT = 0.5;
const MIN_SPLIT = 0.2;
const MAX_SPLIT = 0.8;
const SPLIT_STEP = 0.05;
// Rounded so arrow steps land on whole percents.
const clampSplit = (f: number) => Math.round(Math.min(MAX_SPLIT, Math.max(MIN_SPLIT, f)) * 100) / 100;

function closePort(port: { current: MessagePort | null }) {
  port.current?.close();
  port.current = null;
}

function newNonce(): string {
  // getRandomValues, unlike randomUUID, also works on a plain-HTTP dashboard.
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
}

async function runCall(call: BridgeCall, pluginId: string, host: PageHost): Promise<unknown> {
  switch (call.method) {
    case "ready":
      return null;
    case "sessions.list": {
      const reply = await fetchPluginSessions(pluginId);
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
      host.openSession(call.sessionId);
      return null;
    // Null, not the session row, which is wider than the summary sessions.list hands a page.
    case "session.archive":
      await requireListed(pluginId, call.sessionId);
      if (!(await setSessionArchive(call.sessionId, true))) throw new Error("archive failed");
      return null;
    case "session.trash":
      await requireListed(pluginId, call.sessionId);
      if (!(await trashSession(call.sessionId))) throw new Error("trash failed");
      return null;
    case "session.contextMenu": {
      await requireListed(pluginId, call.sessionId);
      const { sessionId, x, y, items } = call;
      return host.openMenu({ sessionId, x, y, items });
    }
  }
}

/** Only an exact id from sessions.list, which is scoped to the plugin host's profile, reaches the lifecycle API. */
async function requireListed(pluginId: string, sessionId: string) {
  const reply = await fetchPluginSessions(pluginId);
  if (!reply) throw new Error("sessions unavailable");
  if (!reply.sessions.some((s) => s.id === sessionId)) {
    throw new BridgeRefusal("invalid_params", `No session ${sessionId} in this profile.`);
  }
}

/** A plugin page, full width. The frame gets an opaque origin: no dashboard storage, cookies, token or API; the bridge is its only data path. */
export function PluginPageView({
  pluginId,
  pageId,
  onBack,
  readOnly,
  cityhall,
  theme,
  link: rawLink,
  selectedSessionId,
  onSelectSession,
  renderSession,
  renderSessionMenu,
}: Props) {
  const pages = usePluginPages();
  const page = pages?.find((p) => p.plugin_id === pluginId && p.id === pageId);
  const link = page?.deep_link && isDeepLinkValue(rawLink) ? rawLink : null;
  const linkRef = useRef(link);
  const [attempt, setAttempt] = useState(0);
  // Keyed by URL, not the page object, which a poll replaces whenever any plugin's pages change.
  const pageUrl = page?.url;
  const [minted, setMinted] = useState<{ url: string; attempt: number; ok: boolean; nonce: string } | null>(null);
  const [lost, setLost] = useState(false);
  const frameRef = useRef<HTMLIFrameElement | null>(null);
  // A frame's second load is a new document behind the same window, so it no longer holds the handshake.
  const loads = useRef(0);
  // Transferred by the page with `ready`; a later document can never reach it, so nothing crosses a navigation.
  const port = useRef<MessagePort | null>(null);
  // Rides the frame URL's fragment, which never reaches the server; `ready` must echo it, so a document the page navigates to cannot take its identity.
  const nonce = useRef<string | null>(null);

  // Split view: the page's openSession fills a bottom pane instead of leaving the page.
  const wide = useIsWideViewport();
  const [splitPref, toggleSplit] = usePersistedFlag(`aoe-plugin-page-split:${pluginId}/${pageId}`, false);
  const splitOn = wide && splitPref;
  const ticket = page && minted?.url === page.url && minted.attempt === attempt ? minted : null;
  // The split lives inside the framed page; an error in its place takes the session pane with it.
  const framed = !lost && !(pages !== null && !page) && ticket?.ok !== false;
  const split = splitOn && framed;
  const shown = split ? selectedSessionId : null;
  // Keyed by session, so a pane closed while maximized opens the next session in the split.
  const [maximizedId, setMaximizedId] = useState<string | null>(null);
  const maximized = shown !== null && maximizedId === shown;
  const shownRef = useRef(shown);
  const openSessionRef = useRef<(id: string) => void>(requestOpenSession);
  useEffect(() => {
    openSessionRef.current = split ? onSelectSession : requestOpenSession;
  }, [split, onSelectSession]);
  // Off, whether toggled, on a narrowed viewport or with the page gone, the split drops its session, so the dashboard stops treating it as open.
  useEffect(() => {
    // Narrowing and frame loss have no handler to do this in.
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (!split && selectedSessionId) onSelectSession(null);
  }, [split, selectedSessionId, onSelectSession]);
  useEffect(() => {
    shownRef.current = shown;
    port.current?.postMessage(bridgePush("selection", { sessionId: shown }));
  }, [shown]);
  // A second link while the page is open arrives here instead of remounting it.
  useEffect(() => {
    linkRef.current = link;
    port.current?.postMessage(bridgePush("link", { link }));
  }, [link]);

  // The last request stays mounted after its menu closes, so a picker it opened can finish.
  const [menuRequest, setMenuRequest] = useState<SessionMenuRequest | null>(null);
  const menuResolve = useRef<((picked: string | null) => void) | null>(null);
  const menuKey = useRef(0);
  const [host] = useState(() => ({
    openSession: (id: string) => openSessionRef.current(id),
    openMenu: ({ x, y, ...rest }: MenuArgs): Promise<string | null> => {
      // The page sends frame coordinates, kept inside its frame; the menu floats in the dashboard viewport.
      const rect = frameRef.current?.getBoundingClientRect();
      menuResolve.current?.(null);
      return new Promise((resolve) => {
        menuResolve.current = resolve;
        menuKey.current += 1;
        setMenuRequest({
          key: menuKey.current,
          x: (rect?.left ?? 0) + Math.min(x, rect?.width ?? 0),
          y: (rect?.top ?? 0) + Math.min(y, rect?.height ?? 0),
          ...rest,
          onClose: resolve,
        });
      });
    },
  }));

  const attachFrame = useCallback((frame: HTMLIFrameElement | null) => {
    frameRef.current = frame;
    loads.current = 0;
    closePort(port);
  }, []);

  // The frame load authenticates with the ticket this sets, so mint before rendering it.
  useEffect(() => {
    if (!pageUrl) return;
    let cancelled = false;
    void mintPluginPageTicket(pageUrl).then((ok) => {
      if (cancelled) return;
      nonce.current = newNonce();
      setMinted({ url: pageUrl, attempt, ok, nonce: nonce.current });
    });
    return () => {
      cancelled = true;
    };
  }, [pageUrl, attempt]);

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
      // A call that outlives its frame must not open a menu over the next one.
      const live: PageHost = {
        openSession: host.openSession,
        openMenu: (r) =>
          port.current === channel ? host.openMenu({ ...r, source: page.title }) : Promise.resolve(null),
      };
      runCall(resolved.call, page.plugin_id, live).then(
        (result) => channel.postMessage(bridgeReply(call.id, result)),
        (e: unknown) =>
          channel.postMessage(
            e instanceof BridgeRefusal
              ? bridgeError(call.id, e.code, e.message)
              : bridgeError(call.id, "failed", `${call.method} failed.`),
          ),
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
        bridgePush("context", {
          readOnly,
          cityhall,
          pluginId,
          pageId,
          bridgeVersion: BRIDGE_VERSION,
          link: linkRef.current,
        }),
      );
      if (theme) channel.postMessage(bridgePush("theme", themePayload(theme)));
      channel.postMessage(bridgePush("selection", { sessionId: shownRef.current }));
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [page, pluginId, pageId, readOnly, cityhall, theme, host]);

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
      menuResolve.current?.(null);
      setMenuRequest(null);
      setLost(true);
    }
  };

  const reopen = () => {
    setLost(false);
    setAttempt((a) => a + 1);
  };

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
      <SplitLayout
        split={split}
        maximized={maximized}
        top={(dragging) => (
          <iframe
            key={attempt}
            ref={attachFrame}
            title={page.title}
            src={`${page.url}#${ticket.nonce}`}
            sandbox="allow-scripts allow-downloads"
            onLoad={onFrameLoad}
            // The frame would swallow the divider's mousemove events mid-drag.
            className={`flex-1 min-h-0 w-full border-0 bg-surface-900 ${dragging ? "pointer-events-none" : ""}`}
            data-testid="plugin-page-frame"
          />
        )}
        bottom={
          shown ? (
            <SessionPane
              session={renderSession()}
              maximized={maximized}
              onToggleMaximized={() => setMaximizedId(maximized ? null : shown)}
              onOpenFull={() => requestOpenSession(shown)}
              onClose={() => onSelectSession(null)}
            />
          ) : (
            <Empty title="No session selected" detail="Open a session from the page to show it here." />
          )
        }
      />
    );
  }

  return (
    <div className="flex-1 flex flex-col overflow-hidden bg-surface-900" data-testid="plugin-page-view">
      <div className="bg-surface-850 border-b border-surface-700 shrink-0 flex items-center gap-3 px-4 h-12">
        <button onClick={onBack} className="text-brand-500 cursor-pointer text-sm shrink-0">
          &larr; Back
        </button>
        <span className="flex-1 text-xs font-mono text-text-bright truncate">{page?.title ?? pageId}</span>
        {wide && (
          <button
            onClick={toggleSplit}
            className={`w-8 h-8 shrink-0 flex items-center justify-center rounded-md cursor-pointer transition-colors ${
              splitOn
                ? "text-text-primary bg-surface-700/60"
                : "text-text-muted hover:text-text-secondary hover:bg-surface-700/50"
            }`}
            aria-label="Split view"
            aria-pressed={splitOn}
            title="Split view"
          >
            <PanelBottom size={16} strokeWidth={1.5} aria-hidden="true" />
          </button>
        )}
      </div>
      {body}
      {menuRequest && renderSessionMenu(menuRequest)}
    </div>
  );
}

/** The top pane stays at one tree position, so toggling the split never reloads the page frame. */
function SplitLayout({
  split,
  maximized,
  top,
  bottom,
}: {
  split: boolean;
  /** The session pane takes the whole height; the page stays mounted, hidden. */
  maximized: boolean;
  top: (dragging: boolean) => ReactNode;
  bottom: ReactNode;
}) {
  const [fraction, setFraction] = useState(() => {
    const saved = parseFloat(safeGetItem(SPLIT_HEIGHT_KEY) ?? "");
    return Number.isFinite(saved) ? clampSplit(saved) : DEFAULT_SPLIT;
  });
  const [dragging, setDragging] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const latest = useRef(fraction);

  const settle = (next: number) => {
    latest.current = next;
    setFraction(next);
    safeSetItem(SPLIT_HEIGHT_KEY, String(next));
    // Terminals re-fit on resize.
    window.dispatchEvent(new Event("resize"));
  };
  const percent = Math.round(fraction * 100);
  const keySteps: Record<string, number> = {
    ArrowUp: fraction + SPLIT_STEP,
    ArrowDown: fraction - SPLIT_STEP,
    Home: MIN_SPLIT,
    End: MAX_SPLIT,
  };

  useEffect(() => {
    if (!dragging) return;
    const onMove = (e: MouseEvent) => {
      const rect = ref.current?.getBoundingClientRect();
      if (!rect?.height) return;
      latest.current = clampSplit((rect.bottom - e.clientY) / rect.height);
      setFraction(latest.current);
    };
    const onUp = () => {
      setDragging(false);
      safeSetItem(SPLIT_HEIGHT_KEY, String(latest.current));
      window.dispatchEvent(new Event("resize"));
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    document.body.style.cursor = "row-resize";
    document.body.style.userSelect = "none";
    return () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
  }, [dragging]);

  // Terminals re-fit on resize.
  useEffect(() => {
    window.dispatchEvent(new Event("resize"));
  }, [maximized]);

  return (
    <div ref={ref} className="flex-1 min-h-0 flex flex-col">
      <div className={maximized ? "hidden" : "flex-1 min-h-0 flex flex-col"}>{top(dragging)}</div>
      {split && (
        <>
          <div
            hidden={maximized}
            role="separator"
            aria-orientation="horizontal"
            aria-label="Resize split"
            aria-valuemin={MIN_SPLIT * 100}
            aria-valuemax={MAX_SPLIT * 100}
            aria-valuenow={percent}
            tabIndex={0}
            data-testid="plugin-page-split-resize"
            onMouseDown={(e) => {
              e.preventDefault();
              setDragging(true);
            }}
            onKeyDown={(e) => {
              const next = keySteps[e.key];
              if (next === undefined) return;
              e.preventDefault();
              settle(clampSplit(next));
            }}
            className="h-1 shrink-0 cursor-row-resize bg-surface-700/60 hover:bg-brand-600/50 transition-colors duration-75 focus-visible:outline-none focus-visible:bg-brand-500"
          />
          <div
            className="shrink-0 min-h-0 flex flex-col overflow-hidden"
            style={{ height: maximized ? "100%" : `${percent}%` }}
            data-testid="plugin-page-split-session"
          >
            {bottom}
          </div>
        </>
      )}
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

const PANE_BUTTON =
  "w-6 h-6 shrink-0 flex items-center justify-center rounded cursor-pointer text-text-muted hover:text-text-primary hover:bg-surface-700/50";

function SessionPane({
  session,
  maximized,
  onToggleMaximized,
  onOpenFull,
  onClose,
}: {
  session: { title: string; view: ReactNode } | null;
  maximized: boolean;
  onToggleMaximized: () => void;
  onOpenFull: () => void;
  onClose: () => void;
}) {
  const MaxIcon = maximized ? Minimize2 : Maximize2;
  const maxLabel = maximized ? "Restore split" : "Maximize session";
  return (
    <>
      <div className="shrink-0 flex items-center gap-1 pl-3 pr-2 h-8 border-b border-surface-700/60 bg-surface-850">
        <span
          className="flex-1 min-w-0 truncate text-xs font-mono text-text-bright"
          data-testid="plugin-page-split-title"
        >
          {session?.title}
        </span>
        <button
          onClick={onToggleMaximized}
          className={PANE_BUTTON}
          aria-label={maxLabel}
          aria-pressed={maximized}
          title={maxLabel}
        >
          <MaxIcon size={14} aria-hidden="true" />
        </button>
        <button onClick={onOpenFull} className={PANE_BUTTON} aria-label="Open full" title="Open full">
          <ExternalLink size={14} aria-hidden="true" />
        </button>
        <button onClick={onClose} className={PANE_BUTTON} aria-label="Close session" title="Close session">
          <X size={14} aria-hidden="true" />
        </button>
      </div>
      {session?.view ?? <Empty title="Session not found" detail="It may have been deleted." />}
    </>
  );
}
