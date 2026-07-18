/* eslint-disable react-refresh/only-export-components */
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { useServerDown, OFFLINE_TITLE } from "../lib/connectionState";
import { ConnectedDevices } from "./ConnectedDevices";
import { McpServers } from "./McpServers";
import { NotificationSettings } from "./NotificationSettings";
import { SecuritySettings } from "./SecuritySettings";
import { TerminalSettings } from "./TerminalSettings";
import {
  fetchPlugins,
  fetchProfiles,
  fetchSettings,
  getSettingsSchema,
  setDefaultProfile,
  updateProfileSettings,
  updateTheme,
} from "../lib/api";
import { PluginSettingsPage } from "./plugin/PluginSlots";
import type { ProfileInfo, SettingsFieldDescriptor } from "../lib/types";
import { SchemaSection } from "./settings/SchemaSection";
import { SelectField } from "./settings/FormFields";
import { DiffSettings } from "./settings/DiffSettings";
import { TelemetrySettings } from "./settings/TelemetrySettings";
import { PluginsSettings } from "./settings/PluginsSettings";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { PluginSettingsSections } from "./settings/PluginSettingsSections";
import { SettingsHeader } from "./settings/SettingsHeader";
import { ProfilesSection } from "./profiles/ProfilesSection";
import type { SettingsSearchHit } from "./settings/settingsSearchIndex";

export type TabId =
  | "profiles"
  | "session"
  | "sandbox"
  | "worktree"
  | "theme"
  | "diff"
  | "sound"
  | "tmux"
  | "updates"
  | "telemetry"
  | "notifications"
  | "terminal"
  | "security"
  | "devices"
  | "structured-view"
  | "mcp"
  | "logging"
  | "plugins";

// A plugin-contributed settings page (#2985): one nav entry per declared
// `settings-page` UI contribution. The tab id is a parametric string outside the
// closed `TabId` union, so it is kept as `string` here and parsed back with
// `parsePluginPageTab` rather than polluting `ALL_TAB_IDS`/`isTabId`.
export interface PluginPageNav {
  tabId: string;
  label: string;
  pluginId: string;
  contribId: string;
}

const PLUGIN_PAGE_PREFIX = "plugin-page:";

// `plugin-page:<encodedPluginId>:<encodedContribId>`. Each id part is
// percent-encoded, so it carries no literal `:` and the first `:` after the
// prefix is an unambiguous delimiter. Round-trips as a single `/settings/:tab`
// URL segment.
export function pluginPageTabId(pluginId: string, contribId: string): string {
  return `${PLUGIN_PAGE_PREFIX}${encodeURIComponent(pluginId)}:${encodeURIComponent(contribId)}`;
}

export function parsePluginPageTab(tab: string | null): { pluginId: string; contribId: string } | null {
  if (!tab || !tab.startsWith(PLUGIN_PAGE_PREFIX)) return null;
  const rest = tab.slice(PLUGIN_PAGE_PREFIX.length);
  const idx = rest.indexOf(":");
  if (idx < 0) return null;
  try {
    return {
      pluginId: decodeURIComponent(rest.slice(0, idx)),
      contribId: decodeURIComponent(rest.slice(idx + 1)),
    };
  } catch {
    return null;
  }
}

// Derive the settings-page nav entries from the installed-plugin list: one per
// enabled plugin's declared `settings-page` UI contribution. Sorted
// deterministically (name, then contribution id) so the sidebar order is stable
// across reloads. When a plugin declares more than one page, the contribution id
// disambiguates the label.
export function pluginSettingsPages(
  plugins: { id: string; name: string; enabled: boolean; ui_contributions: { slot: string; id: string }[] }[],
): PluginPageNav[] {
  const pages: PluginPageNav[] = [];
  for (const p of plugins) {
    if (!p.enabled) continue;
    const contribs = p.ui_contributions.filter((u) => u.slot === "settings-page");
    for (const c of contribs) {
      pages.push({
        tabId: pluginPageTabId(p.id, c.id),
        label: contribs.length > 1 ? `${p.name}: ${c.id}` : p.name,
        pluginId: p.id,
        contribId: c.id,
      });
    }
  }
  pages.sort((a, b) => a.label.localeCompare(b.label) || a.contribId.localeCompare(b.contribId));
  return pages;
}

type SidebarItem =
  | { kind: "tab"; id: TabId | string; label: string; icon?: ReactNode }
  | { kind: "divider"; label: string };

// ID-card / badge glyph for the Profiles tab. Profiles is the only Settings
// tab that carries an icon; it sits at the top as a meta-section over the
// config tabs below it.
const PROFILES_ICON = (
  <svg
    width="14"
    height="14"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.5"
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden="true"
    className="shrink-0"
  >
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <circle cx="9" cy="10" r="2" />
    <path d="M6 16a3 3 0 0 1 6 0" />
    <path d="M15 9h3" />
    <path d="M15 13h3" />
  </svg>
);

// Sidebar groups mirror the TUI Settings layout (Appearance / Sessions /
// Environment / Notifications / Web Dashboard / System) so muscle memory
// carries across surfaces. The TUI source of truth is
// `categories_for_scope()` in src/tui/settings/mod.rs. Web-only tabs with no
// TUI equivalent (Notifications push, Terminal, Security, Devices) live under
// a "Web Dashboard" divider; TUI-only categories (Agents, Interaction, Hooks,
// StatusHooks) are intentionally not surfaced here. Exported for unit testing
// the exact divider/tab order without fighting the duplicated mobile + desktop
// tab strips in the DOM.
export function buildSidebar(pluginPages: PluginPageNav[] = []): SidebarItem[] {
  const items: SidebarItem[] = [
    { kind: "tab", id: "profiles", label: "Profiles", icon: PROFILES_ICON },
    { kind: "divider", label: "Appearance" },
    { kind: "tab", id: "theme", label: "Theme" },
    { kind: "tab", id: "diff", label: "Diff" },
    { kind: "divider", label: "Sessions" },
    { kind: "tab", id: "session", label: "Session" },
    { kind: "tab", id: "structured-view", label: "Structured view" },
    { kind: "tab", id: "mcp", label: "MCP servers" },
    { kind: "divider", label: "Environment" },
    { kind: "tab", id: "sandbox", label: "Sandbox" },
    { kind: "tab", id: "worktree", label: "Worktree" },
    { kind: "tab", id: "tmux", label: "Tmux" },
    { kind: "divider", label: "Notifications" },
    { kind: "tab", id: "sound", label: "Sound" },
    { kind: "tab", id: "notifications", label: "Notifications" },
    { kind: "divider", label: "Web Dashboard" },
    { kind: "tab", id: "terminal", label: "Terminal" },
    { kind: "tab", id: "security", label: "Security" },
    { kind: "tab", id: "devices", label: "Devices" },
    { kind: "divider", label: "System" },
    { kind: "tab", id: "updates", label: "Updates" },
    { kind: "tab", id: "telemetry", label: "Telemetry" },
    { kind: "tab", id: "logging", label: "Logging" },
    { kind: "tab", id: "plugins", label: "Plugins" },
  ];
  if (pluginPages.length > 0) {
    items.push({ kind: "divider", label: "Plugin pages" });
    for (const page of pluginPages) {
      items.push({ kind: "tab", id: page.tabId, label: page.label });
    }
  }
  return items;
}

interface Props {
  onClose: () => void;
  tab: string | null;
  onSelectTab: (tab: TabId | string) => void;
  onServerAboutRefresh: () => Promise<void> | void;
  onSettingsRefresh?: () => Promise<void> | void;
  /** Profile to preselect, sourced from the `?profile=` query so the
   *  Profiles page can deep-link into a specific profile's section. */
  profile?: string | null;
  /** Notifies the host when the profile changes via the header dropdown,
   *  so it can keep `?profile=` in sync for shareable/refreshable URLs. */
  onSelectProfile?: (profile: string) => void;
  /** Read-only server: the Profiles tab hides its create/edit controls. */
  readOnly?: boolean;
}

const ALL_TAB_IDS = new Set<TabId>([
  "profiles",
  "session",
  "sandbox",
  "worktree",
  "theme",
  "diff",
  "sound",
  "tmux",
  "updates",
  "telemetry",
  "notifications",
  "terminal",
  "security",
  "devices",
  "structured-view",
  "mcp",
  "logging",
  "plugins",
]);

function isTabId(value: unknown): value is TabId {
  return typeof value === "string" && ALL_TAB_IDS.has(value as TabId);
}

// Tabs whose body is rendered (wholly or partly) by the schema-driven
// SchemaSection. They share one loading/error guard so a slow or failed
// `GET /api/settings/schema` shows a single spinner/retry instead of each
// section rendering empty. Tabs absent here are fully hand-written (diff,
// telemetry) or have no config body (terminal, security, devices).
const SCHEMA_BACKED_TABS = new Set<TabId>([
  "session",
  "sandbox",
  "worktree",
  "theme",
  "sound",
  "tmux",
  "updates",
  "logging",
  "notifications",
  "structured-view",
]);

/// Resolves the value `selectedProfile` should take when the mount-time
/// `fetchProfiles()` returns. Preserve a user-set selection if it's still a
/// valid profile (closes the race where the user picks one in the gap before
/// the mount fetch resolves); otherwise fall back to the server's
/// default-flagged profile, then to the literal "default" string. Exported
/// for unit testing because the live race is hard to drive deterministically
/// without mounting all of SettingsView.
export function resolveSelectedProfile(current: string, profiles: ProfileInfo[]): string {
  if (profiles.some((p) => p.name === current)) return current;
  return profiles.find((p) => p.is_default)?.name ?? "default";
}

export function SettingsView({
  onClose,
  tab,
  onSelectTab,
  onServerAboutRefresh,
  onSettingsRefresh = () => {},
  profile,
  onSelectProfile,
  readOnly,
}: Props) {
  const offline = useServerDown();
  const [settings, setSettings] = useState<Record<string, unknown> | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  // Seed empty rather than "default" so the initial
  // useEffect-gated loadSettings doesn't fire a wasted
  // fetchSettings("default") against a profile that may not exist.
  // Once fetchProfiles resolves the seed flips to the real default
  // profile (e.g. "main") and a single loadSettings fires. The
  // previous "default" seed caused two fetchSettings calls (one for
  // the placeholder and one for the resolved name), and the second
  // setSettings could race ahead of an optimistic user edit and
  // clobber it. See #1383 (profile-settings-isolation / settings-
  // tmux-* flakes).
  // Seed from the `?profile=` query (deep-link from the Profiles page) when
  // present, else empty (see the note above on why not "default").
  const [selectedProfile, setSelectedProfile] = useState(profile ?? "");
  // Bumped only on a user-initiated profile switch (the header picker), never
  // on the mount-time fetchProfiles resolution that flips selectedProfile from
  // its "" seed to the default. The content fieldset keys its remount on this
  // epoch (plus activeTab), so resolving the initial profile no longer remounts
  // mid-interaction and collapses a just-expanded "Advanced" fold. Genuine
  // profile switches still remount (reset folds, clear half-typed drafts, break
  // sibling-tab reconciliation), which is what user story #4 wants.
  const [profileEpoch, setProfileEpoch] = useState(0);
  const handleSelectProfile = useCallback(
    (next: string) => {
      setSelectedProfile(next);
      setProfileEpoch((e) => e + 1);
      // Keep ?profile= in sync so the URL stays shareable/refreshable.
      onSelectProfile?.(next);
    },
    [onSelectProfile],
  );
  // Settings pages contributed by installed plugins (#2985), sourced from the
  // manifest ui_contributions (not the live UI-state snapshot) so a nav entry
  // appears on declaration and does not vanish when the worker restarts.
  const [pluginPages, setPluginPages] = useState<PluginPageNav[]>([]);
  // Whether the installed-plugin list has resolved at least once. A parametric
  // plugin-page route that matches no entry is only an invalid route once we
  // know the list is loaded; before that it may just be a not-yet-fetched valid
  // page, so we hold a loading state rather than rejecting it.
  const [pluginsLoaded, setPluginsLoaded] = useState(false);
  const refreshPluginPages = useCallback(
    () =>
      fetchPlugins().then((res) => {
        if (res) setPluginPages(pluginSettingsPages(res.plugins));
        setPluginsLoaded(true);
      }),
    [],
  );
  useEffect(() => {
    void refreshPluginPages();
  }, [refreshPluginPages]);
  const sidebar = buildSidebar(pluginPages);
  const tabs = sidebar.filter((s): s is { kind: "tab"; id: string; label: string } => s.kind === "tab");
  const pluginPageDest = parsePluginPageTab(tab);
  // The declared nav entry a plugin-page route resolves to, or undefined when
  // the route matches no enabled contribution (typo, removed, or disabled).
  const pluginPageNav = pluginPageDest ? pluginPages.find((p) => p.tabId === tab) : undefined;
  const activeTab: TabId = isTabId(tab) ? tab : "session";
  // The nav highlight/label id: the raw parametric tab only for a route that
  // matches a real plugin page, else the resolved built-in TabId (so an invalid
  // plugin-page route highlights the fallback tab, not a phantom entry).
  const activeNavId: string = pluginPageNav ? (tab as string) : activeTab;
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  // Settings schema (single source of truth, #1692). The generic SchemaSection
  // renderer builds sandbox/worktree from this; empty until the one-shot fetch
  // resolves, at which point those tabs populate.
  const [schema, setSchema] = useState<SettingsFieldDescriptor[]>([]);
  const [schemaLoading, setSchemaLoading] = useState(true);
  const [schemaError, setSchemaError] = useState<string | null>(null);
  // Set when a settings-search hit is chosen: switch to the hit's tab and ask
  // the matching SchemaSection to scroll the field into view and highlight it.
  // The nonce bumps on every jump so re-selecting the same field (or jumping to
  // an advanced field on the current tab) re-triggers the scroll and reopens
  // the Advanced fold via the content-subtree remount key.
  const [focusRequest, setFocusRequest] = useState<{ section: string; field: string; nonce: number } | null>(null);
  const handleSearchJump = useCallback(
    (hit: SettingsSearchHit) => {
      setFocusRequest((prev) => ({ section: hit.section, field: hit.field, nonce: (prev?.nonce ?? 0) + 1 }));
      onSelectTab(hit.tab);
    },
    [onSelectTab],
  );

  useEffect(() => {
    fetchProfiles().then((p) => {
      setProfiles(p);
      setSelectedProfile((current) => resolveSelectedProfile(current, p));
    });
  }, []);

  const loadSchema = useCallback(async () => {
    setSchemaLoading(true);
    setSchemaError(null);
    try {
      const s = await getSettingsSchema();
      if (!s) {
        setSchemaError("Failed to load settings schema.");
        return;
      }
      setSchema(s);
    } catch {
      setSchemaError("Failed to load settings schema.");
    } finally {
      setSchemaLoading(false);
    }
  }, []);

  useEffect(() => {
    const timer = setTimeout(() => {
      void loadSchema();
    }, 0);
    return () => clearTimeout(timer);
  }, [loadSchema]);

  // Follow `?profile=` when it changes after mount (e.g. a second deep-link
  // from the Profiles page while Settings stays mounted).
  if (profile && profile !== selectedProfile) {
    setSelectedProfile(profile);
  }

  const defaultProfile = profiles.find((p) => p.is_default)?.name ?? "default";

  const handleSetDefault = async (name: string) => {
    const ok = await setDefaultProfile(name);
    if (ok) fetchProfiles().then(setProfiles);
  };

  // Guard against a slow fetch for a previously-selected profile landing
  // after a fast switch and clobbering the current profile's settings. The
  // Profiles page deep-links raise the odds of rapid profile changes.
  const loadSeq = useRef(0);
  const loadSettings = useCallback(() => {
    if (!selectedProfile) return;
    const seq = ++loadSeq.current;
    fetchSettings(selectedProfile)
      .then((s) => {
        if (seq !== loadSeq.current) return;
        if (s) setSettings(s);
      })
      .catch(() => {
        if (seq !== loadSeq.current) return;
        setSettings(null);
      });
  }, [selectedProfile]);

  useEffect(() => {
    loadSettings();
  }, [loadSettings]);

  const sendSave = useCallback(
    async (section: string, data: Record<string, unknown>): Promise<boolean> => {
      if (!selectedProfile) return false;
      setSaving(true);
      setSaveError(null);
      const ok = await updateProfileSettings(selectedProfile, {
        [section]: data,
      });
      setSaving(false);
      if (!ok) {
        setSaveError("Failed to save, please try again");
        loadSettings();
      }
      return ok;
    },
    [selectedProfile, loadSettings],
  );

  const updateLocal = useCallback(
    (patch: Record<string, unknown>) => {
      if (settings) setSettings({ ...settings, ...patch });
    },
    [settings],
  );

  const session = (settings?.session ?? {}) as Record<string, unknown>;
  const sandbox = (settings?.sandbox ?? {}) as Record<string, unknown>;
  const worktree = (settings?.worktree ?? {}) as Record<string, unknown>;
  const web = (settings?.web ?? {}) as Record<string, unknown>;

  const saveField = useCallback(
    (section: string, sectionData: Record<string, unknown>, field: string, value: unknown): Promise<boolean> => {
      updateLocal({ [section]: { ...sectionData, [field]: value } });
      return sendSave(section, { [field]: value });
    },
    [updateLocal, sendSave],
  );

  const saveSubField = useCallback(
    (section: string, field: string, value: unknown): Promise<boolean> => {
      const sectionData = (settings?.[section] ?? {}) as Record<string, unknown>;
      return saveField(section, sectionData, field, value);
    },
    [settings, saveField],
  );

  // The theme name and color mode are global preferences, not
  // profile-overridable: write them through the dedicated non-elevated
  // /api/theme endpoint instead of the profile settings PATCH. Writing the
  // theme into a profile let a stale override shadow the global pick on every
  // Settings open/close (the empire->rose-pine flip). Profile-overridable rows
  // in the same tab (e.g. idle decay) still write to the selected profile.
  const saveThemeField = useCallback(
    async (section: string, field: string, value: unknown): Promise<boolean> => {
      const overridable = schema.some((d) => d.section === section && d.field === field && d.profile_overridable);
      if (overridable) return saveSubField(section, field, value);
      const sectionData = (settings?.theme ?? {}) as Record<string, unknown>;
      updateLocal({ theme: { ...sectionData, [field]: value } });
      setSaving(true);
      setSaveError(null);
      const ok = await updateTheme({ [field]: value });
      setSaving(false);
      if (!ok) {
        setSaveError("Failed to save, please try again");
        loadSettings();
      }
      return ok;
    },
    [schema, settings, updateLocal, loadSettings, saveSubField],
  );

  const renderTabContent = () => {
    // A plugin settings page (#2985) renders from the plugin UI-state snapshot,
    // not the host `settings`, so it short-circuits before the settings-load
    // guard and the built-in tab switch. Only a route that resolves to a
    // declared, enabled contribution renders the page; an unmatched route waits
    // while the plugin list loads, then falls through to the built-in default
    // rather than showing a permanent "waiting" page for a stale or typo'd URL.
    if (pluginPageDest) {
      if (pluginPageNav) {
        return (
          <PluginSettingsPage
            pluginId={pluginPageDest.pluginId}
            contribId={pluginPageDest.contribId}
            pluginName={pluginPageNav.label}
          />
        );
      }
      if (!pluginsLoaded) {
        return <div className="text-sm text-text-dim">Loading settings...</div>;
      }
      // Loaded with no match: fall through to the built-in default tab.
    }
    if (
      !settings &&
      activeTab !== "profiles" &&
      activeTab !== "notifications" &&
      activeTab !== "terminal" &&
      activeTab !== "security" &&
      activeTab !== "devices" &&
      activeTab !== "structured-view" &&
      activeTab !== "mcp" &&
      activeTab !== "plugins" &&
      activeTab !== "telemetry"
    ) {
      return <div className="text-sm text-text-dim">Loading settings...</div>;
    }

    // The spinner/retry shown in place of a SchemaSection while the schema
    // loads or after it fails. Returns null once the schema is ready.
    const schemaGuard = () => {
      if (schemaLoading) {
        return <div className="text-sm text-text-dim">Loading settings schema...</div>;
      }
      if (schemaError) {
        return (
          <div className="space-y-3">
            <div className="text-sm text-status-error">{schemaError}</div>
            <button
              type="button"
              onClick={() => void loadSchema()}
              className="rounded px-3 py-1 text-xs font-medium bg-surface-700 text-text-secondary hover:bg-surface-600 cursor-pointer"
            >
              Retry
            </button>
          </div>
        );
      }
      return null;
    };

    // Pure schema tabs (whole body is one SchemaSection) short-circuit on the
    // guard. Mixed tabs (session, notifications) render their non-schema rows
    // regardless and guard only the SchemaSection slot, so a slow or failed
    // schema fetch never hides the default-profile selector or the push block.
    if (SCHEMA_BACKED_TABS.has(activeTab) && activeTab !== "session" && activeTab !== "notifications") {
      const guard = schemaGuard();
      if (guard) return guard;
    }

    switch (activeTab) {
      case "profiles":
        return <ProfilesSection readOnly={readOnly} />;

      case "session":
        return (
          <div className="space-y-4">
            {/* Non-schema row: choosing the default profile is a profile-
                management action, not a config field. */}
            <SelectField
              label="Default profile"
              description="Profile used for new sessions"
              value={defaultProfile}
              onChange={(v) => handleSetDefault(v)}
              options={profiles.map((p) => ({ value: p.name, label: p.name }))}
            />
            {/* acp_defaults (Structured View Defaults) is now schema-driven via
                the acp-defaults custom widget, so it renders inside this
                SchemaSection alongside the rest of the session fields. The
                guard covers only the schema rows; the selector above always
                shows. */}
            {schemaGuard() ?? (
              <SchemaSection
                section="session"
                schema={schema}
                focusRequest={focusRequest}
                values={session}
                onSaveField={saveSubField}
                onAfterSave={(descriptor) => {
                  if (descriptor.field === "row_tag") return onSettingsRefresh();
                }}
                advancedSubtitle="Idle auto-stop, attach modes, live-send, and other session tuning."
              />
            )}
          </div>
        );

      case "sandbox":
        return (
          <SchemaSection
            section="sandbox"
            schema={schema}
            focusRequest={focusRequest}
            values={sandbox}
            onSaveField={saveSubField}
            advancedSubtitle="Resource limits, custom instructions, environment, volumes, and ports."
          />
        );

      case "worktree":
        return (
          <SchemaSection
            section="worktree"
            schema={schema}
            focusRequest={focusRequest}
            values={worktree}
            onSaveField={saveSubField}
            advancedSubtitle="Bare-repo and workspace path templates, branch cleanup, and submodules."
            fieldAnchor={{ field: "path_template", anchor: TOUR_ANCHORS.settingsWorktree }}
          />
        );

      case "theme":
        return (
          <SchemaSection
            section="theme"
            schema={schema}
            focusRequest={focusRequest}
            values={(settings?.theme ?? {}) as Record<string, unknown>}
            onSaveField={saveThemeField}
          />
        );
      case "diff":
        return <DiffSettings />;
      case "sound":
        return (
          <SchemaSection
            section="sound"
            schema={schema}
            focusRequest={focusRequest}
            values={(settings?.sound ?? {}) as Record<string, unknown>}
            onSaveField={saveSubField}
          />
        );
      case "tmux":
        return (
          <SchemaSection
            section="tmux"
            schema={schema}
            focusRequest={focusRequest}
            values={(settings?.tmux ?? {}) as Record<string, unknown>}
            onSaveField={saveSubField}
          />
        );
      case "updates":
        return (
          <SchemaSection
            section="updates"
            schema={schema}
            focusRequest={focusRequest}
            values={(settings?.updates ?? {}) as Record<string, unknown>}
            onSaveField={saveSubField}
          />
        );
      case "telemetry":
        return <TelemetrySettings />;
      case "logging":
        return (
          <SchemaSection
            section="logging"
            schema={schema}
            focusRequest={focusRequest}
            values={(settings?.logging ?? {}) as Record<string, unknown>}
            onSaveField={saveSubField}
            advancedSubtitle="Sink and rotation; some fields require restarting aoe to take effect."
          />
        );

      case "plugins":
        return (
          <div className="space-y-6" {...tourAnchor(TOUR_ANCHORS.settingsPlugins)}>
            <PluginsSettings onPluginsChanged={refreshPluginPages} />
            {schemaGuard() ?? <PluginSettingsSections schema={schema} settings={settings} onSaved={loadSettings} />}
          </div>
        );

      case "notifications":
        return (
          <div className="space-y-6">
            {/* Browser-push controls render regardless of schema state. */}
            <NotificationSettings />
            <div className="space-y-4">
              <h4 className="text-xs font-mono uppercase tracking-widest text-text-muted">Server Defaults</h4>
              <p className="text-xs text-text-dim">
                Controls which session events trigger push notifications on the server.
              </p>
              {schemaGuard() ??
                (settings && (
                  <SchemaSection
                    section="web"
                    schema={schema}
                    focusRequest={focusRequest}
                    values={web}
                    onSaveField={saveSubField}
                  />
                ))}
            </div>
          </div>
        );

      case "terminal":
        return <TerminalSettings />;
      case "security":
        return <SecuritySettings />;
      case "devices":
        return <ConnectedDevices />;
      case "mcp":
        return <McpServers />;
      case "structured-view": {
        if (!settings) {
          return <div className="text-sm text-text-dim">Loading settings...</div>;
        }
        const acp = (settings.acp ?? {}) as Record<string, unknown>;
        return (
          <div className="space-y-4">
            {/* Tour anchor for the per-agent defaults step (#2631). Anchored on
                this top-of-tab intro, not the defaults widget itself, so
                react-joyride never has to scroll a far-down, async-growing
                target into view (which made it loop and never advance). */}
            <p className="text-xs text-text-dim" {...tourAnchor(TOUR_ANCHORS.settingsAgentDefaults)}>
              Defaults for structured-view (ACP) sessions: which agent starts, how many workers run at once, how much
              history is replayed on reconnect, and the per-agent model, mode, and thinking defaults below. These apply
              when a session renders in the structured view instead of a raw terminal.
            </p>
            <SchemaSection
              section="acp"
              schema={schema}
              focusRequest={focusRequest}
              values={acp}
              onSaveField={saveSubField}
              // The acp section mirrors three fields into serverAbout, which
              // ToolCards and the composer read live; refresh it after any acp
              // save so those surfaces pick up the change without a reload.
              onAfterSave={() => onServerAboutRefresh()}
              advancedSubtitle="Replay retention caps and daemon watchdog tuning. Touch only when triaging a specific failure mode."
            />
          </div>
        );
      }
    }
  };

  const currentTabLabel = tabs.find((t) => t.id === activeNavId)?.label ?? "";

  return (
    <div className="flex-1 flex flex-col overflow-hidden bg-surface-900">
      <SettingsHeader
        onClose={onClose}
        saving={saving}
        saveError={saveError}
        selectedProfile={selectedProfile}
        onSelectProfile={handleSelectProfile}
        schema={schema}
        schemaLoading={schemaLoading}
        onSearchJump={handleSearchJump}
      />

      {/* Mobile tabs (horizontal scroll) */}
      <div className="md:hidden border-b border-surface-700 bg-surface-850 overflow-x-auto">
        <div className="flex items-center">
          {sidebar.map((item) =>
            item.kind === "divider" ? (
              <div key={item.label} className="h-4 w-px bg-surface-700 mx-1 shrink-0" />
            ) : (
              <button
                key={item.id}
                onClick={() => onSelectTab(item.id)}
                className={`flex items-center gap-1.5 px-4 py-2.5 text-xs font-medium whitespace-nowrap cursor-pointer transition-colors ${
                  activeNavId === item.id
                    ? "text-brand-500 border-b-2 border-brand-500"
                    : "text-text-secondary hover:text-text-primary"
                }`}
              >
                {item.icon}
                {item.label}
              </button>
            ),
          )}
        </div>
      </div>

      {/* Desktop: sidebar tabs + content */}
      <div className="flex-1 flex min-h-0">
        {/* Side tabs (desktop only) */}
        <nav className="hidden md:flex flex-col w-44 shrink-0 border-r border-surface-700 bg-surface-850 py-2 overflow-y-auto">
          {sidebar.map((item, i) =>
            item.kind === "divider" ? (
              <div
                key={item.label}
                className={`px-4 pt-3 pb-1 text-[10px] font-mono uppercase tracking-widest text-text-dim ${i > 0 ? "mt-2 border-t border-surface-700/40" : ""}`}
              >
                {item.label}
              </div>
            ) : (
              <button
                key={item.id}
                onClick={() => onSelectTab(item.id)}
                className={`flex items-center gap-2 px-4 py-2 text-sm text-left cursor-pointer transition-colors ${
                  activeNavId === item.id
                    ? "text-brand-500 bg-surface-800 border-r-2 border-brand-500"
                    : "text-text-secondary hover:text-text-primary hover:bg-surface-800/50"
                }`}
              >
                {item.icon}
                {item.label}
              </button>
            ),
          )}
        </nav>

        {/* Content area */}
        <div className="flex-1 overflow-y-auto">
          <div className="p-6 max-w-2xl mx-auto space-y-5">
            <h2 className="text-lg font-semibold text-text-bright">{currentTabLabel}</h2>

            {offline && (
              <div className="text-sm text-status-error bg-status-error/10 rounded-lg p-3">
                {OFFLINE_TITLE}: toggles will not save while disconnected.
              </div>
            )}
            {/* Keying on tab + profileEpoch remounts the content subtree on a
                tab switch or a user-initiated profile switch, which resets every
                component-local <CollapsibleSection> "Advanced" fold back to
                collapsed (user story #4) and clears any half-typed field draft so
                it cannot blur-commit into the wrong profile. It also breaks React
                reconciliation between sibling tabs that share the same root
                element shape, e.g. sandbox and worktree both rendering <div
                className="space-y-4">. profileEpoch (not selectedProfile) is used
                so the mount-time fetchProfiles resolution that flips
                selectedProfile from its "" seed to the default does not remount
                mid-interaction and collapse a just-expanded fold. */}
            <fieldset
              key={`${activeNavId}-${profileEpoch}-${focusRequest?.nonce ?? 0}`}
              disabled={offline}
              className="space-y-5 disabled:opacity-50 border-0 m-0 p-0 min-w-0"
            >
              {renderTabContent()}
            </fieldset>
          </div>
        </div>
      </div>
    </div>
  );
}
