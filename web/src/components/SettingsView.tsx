import { useCallback, useEffect, useState } from "react";
import { useServerDown, OFFLINE_TITLE } from "../lib/connectionState";
import { ConnectedDevices } from "./ConnectedDevices";
import { NotificationSettings } from "./NotificationSettings";
import { SecuritySettings } from "./SecuritySettings";
import { TerminalSettings } from "./TerminalSettings";
import {
  fetchProfiles,
  fetchSettings,
  setCockpitMaster,
  setDefaultProfile,
  updateProfileSettings,
  type ServerAbout,
} from "../lib/api";
import type { ProfileInfo } from "../lib/types";
import {
  CollapsibleSection,
  ListField,
  NumberField,
  SelectField,
  TextField,
  ToggleField,
} from "./settings/FormFields";
import { ThemeSettings } from "./settings/ThemeSettings";
import { SoundSettings } from "./settings/SoundSettings";
import { UpdateSettings } from "./settings/UpdateSettings";
import { TmuxSettings } from "./settings/TmuxSettings";
import { LoggingSettings } from "./settings/LoggingSettings";
import { SettingsHeader } from "./settings/SettingsHeader";

type TabId =
  | "session"
  | "sandbox"
  | "worktree"
  | "theme"
  | "sound"
  | "tmux"
  | "updates"
  | "notifications"
  | "terminal"
  | "security"
  | "devices"
  | "cockpit"
  | "logging";

type SidebarItem =
  | { kind: "tab"; id: TabId; label: string }
  | { kind: "divider"; label: string };

// Sidebar groups mirror the TUI Settings layout (Appearance / Sessions /
// Environment / Notifications / Web Dashboard / System) so muscle memory
// carries across surfaces. The TUI source of truth is
// `categories_for_scope()` in src/tui/settings/mod.rs. Web-only tabs with no
// TUI equivalent (Notifications push, Terminal, Security, Devices) live under
// a "Web Dashboard" divider; TUI-only categories (Agents, Interaction, Hooks,
// StatusHooks) are intentionally not surfaced here. Exported for unit testing
// the exact divider/tab order without fighting the duplicated mobile + desktop
// tab strips in the DOM.
export function buildSidebar(): SidebarItem[] {
  return [
    { kind: "divider", label: "Appearance" },
    { kind: "tab", id: "theme", label: "Theme" },
    { kind: "divider", label: "Sessions" },
    { kind: "tab", id: "session", label: "Session" },
    { kind: "tab", id: "cockpit", label: "Cockpit" },
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
    { kind: "tab", id: "logging", label: "Logging" },
  ];
}

interface Props {
  onClose: () => void;
  tab: string | null;
  onSelectTab: (tab: TabId) => void;
  serverAbout: ServerAbout | null;
  onServerAboutRefresh: () => Promise<void> | void;
}

const ALL_TAB_IDS = new Set<TabId>([
  "session",
  "sandbox",
  "worktree",
  "theme",
  "sound",
  "tmux",
  "updates",
  "notifications",
  "terminal",
  "security",
  "devices",
  "cockpit",
  "logging",
]);

function isTabId(value: unknown): value is TabId {
  return typeof value === "string" && ALL_TAB_IDS.has(value as TabId);
}

/// Resolves the value `selectedProfile` should take when the mount-time
/// `fetchProfiles()` returns. Preserve a user-set selection if it's still a
/// valid profile (closes the race where the user picks one in the gap before
/// the mount fetch resolves); otherwise fall back to the server's
/// default-flagged profile, then to the literal "default" string. Exported
/// for unit testing because the live race is hard to drive deterministically
/// without mounting all of SettingsView.
export function resolveSelectedProfile(
  current: string,
  profiles: ProfileInfo[],
): string {
  if (profiles.some((p) => p.name === current)) return current;
  return profiles.find((p) => p.is_default)?.name ?? "default";
}

export function SettingsView({
  onClose,
  tab,
  onSelectTab,
  serverAbout,
  onServerAboutRefresh,
}: Props) {
  const offline = useServerDown();
  const [settings, setSettings] = useState<Record<string, unknown> | null>(
    null,
  );
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
  const [selectedProfile, setSelectedProfile] = useState("");
  // Bumped only on a user-initiated profile switch (the header picker), never
  // on the mount-time fetchProfiles resolution that flips selectedProfile from
  // its "" seed to the default. The content fieldset keys its remount on this
  // epoch (plus activeTab), so resolving the initial profile no longer remounts
  // mid-interaction and collapses a just-expanded "Advanced" fold. Genuine
  // profile switches still remount (reset folds, clear half-typed drafts, break
  // sibling-tab reconciliation), which is what user story #4 wants.
  const [profileEpoch, setProfileEpoch] = useState(0);
  const handleSelectProfile = useCallback((profile: string) => {
    setSelectedProfile(profile);
    setProfileEpoch((e) => e + 1);
  }, []);
  const sidebar = buildSidebar();
  const tabs = sidebar.filter(
    (s): s is { kind: "tab"; id: TabId; label: string } => s.kind === "tab",
  );
  const activeTab: TabId = isTabId(tab) ? tab : "session";
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);

  useEffect(() => {
    fetchProfiles().then((p) => {
      setProfiles(p);
      setSelectedProfile((current) => resolveSelectedProfile(current, p));
    });
  }, []);

  const defaultProfile = profiles.find((p) => p.is_default)?.name ?? "default";

  const handleSetDefault = async (name: string) => {
    const ok = await setDefaultProfile(name);
    if (ok) fetchProfiles().then(setProfiles);
  };

  const loadSettings = useCallback(() => {
    if (!selectedProfile) return;
    fetchSettings(selectedProfile).then((s) => {
      if (s) setSettings(s);
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
      const ok = await updateProfileSettings(selectedProfile, { [section]: data });
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

  const saveField = (
    section: string,
    sectionData: Record<string, unknown>,
    field: string,
    value: unknown,
  ): Promise<boolean> => {
    updateLocal({ [section]: { ...sectionData, [field]: value } });
    return sendSave(section, { [field]: value });
  };

  const saveSubField = useCallback(
    (section: string, field: string, value: unknown): Promise<boolean> => {
      const sectionData = (settings?.[section] ?? {}) as Record<string, unknown>;
      return saveField(section, sectionData, field, value);
    },
    [settings, selectedProfile, sendSave, loadSettings],
  );

  const renderTabContent = () => {
    if (!settings && activeTab !== "notifications" && activeTab !== "terminal" && activeTab !== "security" && activeTab !== "devices" && activeTab !== "cockpit") {
      return <div className="text-sm text-text-dim">Loading settings...</div>;
    }

    switch (activeTab) {
      case "session":
        return (
          <div className="space-y-4">
            <SelectField
              label="Default profile"
              description="Profile used for new sessions"
              value={defaultProfile}
              onChange={(v) => handleSetDefault(v)}
              options={profiles.map((p) => ({ value: p.name, label: p.name }))}
            />
            <TextField
              label="Default agent"
              value={(session.default_tool as string) ?? ""}
              onChange={(v) => saveField("session", session, "default_tool", v || null)}
              placeholder="Auto-detect"
              mono
            />
            <ToggleField
              label="YOLO mode by default"
              description="New sessions skip permission prompts"
              checked={(session.yolo_mode_default as boolean) ?? false}
              onChange={(v) => saveField("session", session, "yolo_mode_default", v)}
            />
            <ToggleField
              label="Strict hotkeys"
              description="Require SHIFT on letter-based TUI hotkeys to prevent accidental actions"
              checked={(session.strict_hotkeys as boolean) ?? false}
              onChange={(v) => saveField("session", session, "strict_hotkeys", v)}
            />
            <ToggleField
              label="Agent status hooks"
              description="Install status-detection hooks into agent settings files for reliable status tracking"
              checked={(session.agent_status_hooks as boolean) ?? true}
              onChange={(v) => saveField("session", session, "agent_status_hooks", v)}
            />
          </div>
        );

      case "sandbox":
        return (
          <div className="space-y-4">
            <ToggleField
              label="Sandbox enabled by default"
              description="Run new sessions in a Docker container"
              checked={(sandbox.enabled_by_default as boolean) ?? false}
              onChange={(v) => saveField("sandbox", sandbox, "enabled_by_default", v)}
            />
            <TextField
              label="Default container image"
              value={(sandbox.default_image as string) ?? ""}
              onChange={(v) => saveField("sandbox", sandbox, "default_image", v)}
              placeholder="ghcr.io/agent-of-empires/aoe-sandbox:latest"
              mono
            />
            <SelectField
              label="Default terminal mode"
              value={(sandbox.default_terminal_mode as string) ?? "host"}
              onChange={(v) => saveField("sandbox", sandbox, "default_terminal_mode", v)}
              options={[
                { value: "host", label: "Host" },
                { value: "container", label: "Container" },
              ]}
            />
            <SelectField
              label="Container runtime"
              value={(sandbox.container_runtime as string) ?? "docker"}
              onChange={(v) => saveField("sandbox", sandbox, "container_runtime", v)}
              options={[
                { value: "docker", label: "Docker" },
                { value: "apple_container", label: "Apple Container" },
              ]}
            />
            <ToggleField
              label="Mount SSH keys"
              description="Mount ~/.ssh into sandbox containers"
              checked={(sandbox.mount_ssh as boolean) ?? false}
              onChange={(v) => saveField("sandbox", sandbox, "mount_ssh", v)}
            />
            <ToggleField
              label="Auto cleanup"
              description="Remove containers when sessions are deleted"
              checked={(sandbox.auto_cleanup as boolean) ?? true}
              onChange={(v) => saveField("sandbox", sandbox, "auto_cleanup", v)}
            />
            <CollapsibleSection
              title="Advanced"
              subtitle="Resource limits, custom instructions, environment, volumes, and ports."
            >
              <TextField
                label="CPU limit"
                value={(sandbox.cpu_limit as string) ?? ""}
                onChange={(v) => saveField("sandbox", sandbox, "cpu_limit", v || null)}
                placeholder="e.g. 4"
              />
              <TextField
                label="Memory limit"
                value={(sandbox.memory_limit as string) ?? ""}
                onChange={(v) => saveField("sandbox", sandbox, "memory_limit", v || null)}
                placeholder="e.g. 8g"
              />
              <TextField
                label="Custom instruction"
                description="Text appended to the agent system prompt in sandboxed sessions"
                value={(sandbox.custom_instruction as string) ?? ""}
                onChange={(v) => saveField("sandbox", sandbox, "custom_instruction", v || null)}
                placeholder="Additional instructions for the agent..."
                multiline
              />
              <ListField
                label="Environment variables"
                description="Variables passed to sandbox containers (KEY or KEY=VALUE)"
                items={(sandbox.environment as string[]) ?? []}
                onChange={(items) => saveField("sandbox", sandbox, "environment", items)}
                placeholder="KEY or KEY=VALUE"
                validate={(v) => {
                  if (!/^[A-Za-z_][A-Za-z0-9_]*(=.*)?$/.test(v))
                    return "Must be KEY or KEY=VALUE (letters, digits, underscores)";
                  return null;
                }}
              />
              <ListField
                label="Extra volumes"
                description="Additional volume mounts (host:container[:ro])"
                items={(sandbox.extra_volumes as string[]) ?? []}
                onChange={(items) => saveField("sandbox", sandbox, "extra_volumes", items)}
                placeholder="/host/path:/container/path"
                validate={(v) => {
                  if (!v.includes(":")) return "Must contain ':' (host:container)";
                  return null;
                }}
              />
              <ListField
                label="Port mappings"
                description="Port forwarding (host:container)"
                items={(sandbox.port_mappings as string[]) ?? []}
                onChange={(items) => saveField("sandbox", sandbox, "port_mappings", items)}
                placeholder="3000:3000"
                validate={(v) => {
                  if (!/^\d+:\d+$/.test(v)) return "Must be port:port (e.g. 3000:3000)";
                  return null;
                }}
              />
              <ListField
                label="Volume ignores"
                description="Directories excluded from host bind mount"
                items={(sandbox.volume_ignores as string[]) ?? []}
                onChange={(items) => saveField("sandbox", sandbox, "volume_ignores", items)}
                placeholder="node_modules"
              />
            </CollapsibleSection>
          </div>
        );

      case "worktree":
        return (
          <div className="space-y-4">
            <ToggleField
              label="Worktrees enabled"
              description="Create git worktrees for new sessions"
              checked={(worktree.enabled as boolean) ?? false}
              onChange={(v) => saveField("worktree", worktree, "enabled", v)}
            />
            <TextField
              label="Path template"
              description="Template for worktree directories in regular repos ({repo-name}, {branch})"
              value={(worktree.path_template as string) ?? ""}
              onChange={(v) => saveField("worktree", worktree, "path_template", v)}
              placeholder="../{repo-name}-worktrees/{branch}"
              mono
            />
            <ToggleField
              label="Auto cleanup"
              description="Delete worktrees when sessions are removed"
              checked={(worktree.auto_cleanup as boolean) ?? true}
              onChange={(v) => saveField("worktree", worktree, "auto_cleanup", v)}
            />
            <CollapsibleSection
              title="Advanced"
              subtitle="Bare-repo and workspace path templates, branch cleanup, and submodules."
            >
              <TextField
                label="Bare repo path template"
                description="Template for worktree directories in bare repos ({branch})"
                value={(worktree.bare_repo_path_template as string) ?? ""}
                onChange={(v) => saveField("worktree", worktree, "bare_repo_path_template", v)}
                placeholder="./{branch}"
                mono
              />
              <TextField
                label="Workspace path template"
                description="Template for multi-repo workspace directories ({branch}, {session-id})"
                value={(worktree.workspace_path_template as string) ?? ""}
                onChange={(v) => saveField("worktree", worktree, "workspace_path_template", v)}
                placeholder="../{branch}-workspace-{session-id}"
                mono
              />
              <ToggleField
                label="Delete branch on cleanup"
                description="Also delete the git branch when cleaning up a worktree"
                checked={(worktree.delete_branch_on_cleanup as boolean) ?? false}
                onChange={(v) => saveField("worktree", worktree, "delete_branch_on_cleanup", v)}
              />
              <ToggleField
                label="Init submodules"
                description="Run `git submodule update --init --recursive` after creating a worktree"
                checked={(worktree.init_submodules as boolean) ?? true}
                onChange={(v) => saveField("worktree", worktree, "init_submodules", v)}
              />
            </CollapsibleSection>
          </div>
        );

      case "theme":
        return <ThemeSettings settings={settings!} onSaveField={saveSubField} onUpdate={updateLocal} />;
      case "sound":
        return <SoundSettings settings={settings!} onSaveField={saveSubField} onUpdate={updateLocal} />;
      case "tmux":
        return <TmuxSettings settings={settings!} onSaveField={saveSubField} onUpdate={updateLocal} />;
      case "updates":
        return <UpdateSettings settings={settings!} onSaveField={saveSubField} onUpdate={updateLocal} />;
      case "logging":
        return <LoggingSettings settings={settings!} onSaveField={saveSubField} onUpdate={updateLocal} />;

      case "notifications":
        return (
          <div className="space-y-6">
            <NotificationSettings />
            {settings && (
              <div className="space-y-4">
                <h4 className="text-xs font-mono uppercase tracking-widest text-text-muted">
                  Server Defaults
                </h4>
                <p className="text-xs text-text-dim">
                  Controls which session events trigger push notifications on the server.
                </p>
                <ToggleField
                  label="Push notifications enabled"
                  description="Server-wide kill switch for push notifications"
                  checked={(web.notifications_enabled as boolean) ?? true}
                  onChange={(v) => saveField("web", web, "notifications_enabled", v)}
                />
                <ToggleField
                  label="Notify on waiting"
                  description="Send push when a session needs input"
                  checked={(web.notify_on_waiting as boolean) ?? true}
                  onChange={(v) => saveField("web", web, "notify_on_waiting", v)}
                />
                <ToggleField
                  label="Notify on idle"
                  description="Send push when a session finishes"
                  checked={(web.notify_on_idle as boolean) ?? false}
                  onChange={(v) => saveField("web", web, "notify_on_idle", v)}
                />
                <ToggleField
                  label="Notify on error"
                  description="Send push when a session errors"
                  checked={(web.notify_on_error as boolean) ?? true}
                  onChange={(v) => saveField("web", web, "notify_on_error", v)}
                />
              </div>
            )}
          </div>
        );

      case "terminal":
        return <TerminalSettings />;
      case "security":
        return <SecuritySettings />;
      case "devices":
        return <ConnectedDevices />;
      case "cockpit": {
        const cockpit = (settings?.cockpit ?? {}) as Record<string, unknown>;
        return (
          <CockpitSettings
            serverAbout={serverAbout}
            onRefresh={onServerAboutRefresh}
            cockpit={cockpit}
            onSaveField={saveSubField}
          />
        );
      }
    }
  };

  const currentTabLabel = tabs.find((t) => t.id === activeTab)?.label ?? "";

  return (
    <div className="flex-1 flex flex-col overflow-hidden bg-surface-900">
      <SettingsHeader
        onClose={onClose}
        saving={saving}
        saveError={saveError}
        selectedProfile={selectedProfile}
        onSelectProfile={handleSelectProfile}
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
                className={`px-4 py-2.5 text-xs font-medium whitespace-nowrap cursor-pointer transition-colors ${
                  activeTab === item.id
                    ? "text-brand-500 border-b-2 border-brand-500"
                    : "text-text-secondary hover:text-text-primary"
                }`}
              >
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
                className={`px-4 py-2 text-sm text-left cursor-pointer transition-colors ${
                  activeTab === item.id
                    ? "text-brand-500 bg-surface-800 border-r-2 border-brand-500"
                    : "text-text-secondary hover:text-text-primary hover:bg-surface-800/50"
                }`}
              >
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
              key={`${activeTab}-${profileEpoch}`}
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

function CockpitSettings({
  serverAbout,
  onRefresh,
  cockpit,
  onSaveField,
}: {
  serverAbout: ServerAbout | null;
  onRefresh: () => Promise<void> | void;
  cockpit: Record<string, unknown>;
  onSaveField: (section: string, field: string, value: unknown) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const masterEnabled = !!serverAbout?.cockpit_master_enabled;
  // Local mirror so the toggle reflects optimistically while the
  // backend save + /api/about re-fetch propagate.
  const showToolDurations =
    typeof cockpit.show_tool_durations === "boolean"
      ? (cockpit.show_tool_durations as boolean)
      : (serverAbout?.cockpit_show_tool_durations ?? true);
  const queueDrainMode: "combined" | "serial" =
    cockpit.queue_drain_mode === "serial" || cockpit.queue_drain_mode === "combined"
      ? (cockpit.queue_drain_mode as "combined" | "serial")
      : (serverAbout?.cockpit_queue_drain_mode ?? "combined");
  const maxConcurrentResumes =
    typeof cockpit.max_concurrent_resumes === "number"
      ? (cockpit.max_concurrent_resumes as number)
      : (serverAbout?.cockpit_max_concurrent_resumes ?? 4);

  const onToggle = async (next: boolean) => {
    setBusy(true);
    setError(null);
    const res = await setCockpitMaster(next);
    setBusy(false);
    if (!res) {
      setError("Failed to update; check server logs");
      return;
    }
    await onRefresh();
  };

  return (
    <div className="space-y-4">
      <div className="rounded border border-surface-700 bg-surface-800/40 p-3 text-xs text-text-dim space-y-1">
        <div>
          <span className="text-text-muted">Status:</span>{" "}
          {masterEnabled ? (
            <span className="text-emerald-400">Cockpit available for new sessions</span>
          ) : (
            <span className="text-amber-400">Cockpit disabled</span>
          )}
        </div>
        <div>
          <span className="text-text-muted">cockpit.enabled:</span>{" "}
          <code className="rounded bg-surface-900 px-1">{masterEnabled ? "true" : "false"}</code>
        </div>
      </div>

      <div className="flex items-start justify-between gap-3 py-1">
        <div>
          <div className="text-sm text-text-bright">Cockpit master switch</div>
          <div className="text-xs text-text-dim mt-0.5">
            Persists to <code>config.toml</code> as <code>cockpit.enabled</code>; takes effect immediately.
          </div>
        </div>
        <button
          type="button"
          disabled={busy}
          onClick={() => onToggle(!masterEnabled)}
          className={`shrink-0 rounded px-3 py-1 text-xs font-medium transition-colors ${
            masterEnabled
              ? "bg-brand-500 text-white hover:bg-brand-400"
              : "bg-surface-700 text-text-secondary hover:bg-surface-600"
          } ${busy ? "opacity-50 cursor-not-allowed" : "cursor-pointer"}`}
        >
          {masterEnabled ? "Enabled" : "Disabled"}
        </button>
      </div>

      <div className="flex items-start justify-between gap-3 py-1 border-t border-surface-800 pt-3">
        <div>
          <div className="text-sm text-text-bright">Show tool-call durations</div>
          <div className="text-xs text-text-dim mt-0.5">
            Persists to <code>config.toml</code> as{" "}
            <code>cockpit.show_tool_durations</code>; cross-device. Renders the elapsed-time label on every
            cockpit tool card. The underlying measurement is currently imprecise on{" "}
            <code>claude-agent-acp</code> (no <code>status: in_progress</code> signal); durations include
            stream-arrival skew rather than just runtime, so for example a parallel{" "}
            <code>sleep 1</code> can read as ~3 s. Turn off if the inflated numbers are more confusing than
            useful.
          </div>
        </div>
        <button
          type="button"
          aria-pressed={showToolDurations}
          aria-label="Show tool-call durations"
          onClick={async () => {
            const next = !showToolDurations;
            onSaveField("cockpit", "show_tool_durations", next);
            await onRefresh();
          }}
          className={`shrink-0 rounded px-3 py-1 text-xs font-medium transition-colors cursor-pointer ${
            showToolDurations
              ? "bg-brand-500 text-white hover:bg-brand-400"
              : "bg-surface-700 text-text-secondary hover:bg-surface-600"
          }`}
        >
          {showToolDurations ? "Visible" : "Hidden"}
        </button>
      </div>

      <div className="flex items-start justify-between gap-3 py-1 border-t border-surface-800 pt-3">
        <div>
          <div className="text-sm text-text-bright">Queue drain mode</div>
          <div className="text-xs text-text-dim mt-0.5">
            Persists to <code>config.toml</code> as{" "}
            <code>cockpit.queue_drain_mode</code>; cross-device. Controls how follow-up prompts queued
            while the agent is busy get dispatched once the current turn ends. <strong>Combined</strong>{" "}
            (default) joins every queued entry with a blank line and sends them as a single prompt; one
            response covers the whole batch. <strong>Serial</strong> fires one entry at a time and waits
            for each response before sending the next.
          </div>
        </div>
        <div className="shrink-0 inline-flex rounded border border-surface-700 bg-surface-900/50 p-0.5 text-xs font-medium">
          {(["combined", "serial"] as const).map((opt) => (
            <button
              key={opt}
              type="button"
              aria-pressed={queueDrainMode === opt}
              onClick={async () => {
                if (queueDrainMode === opt) return;
                onSaveField("cockpit", "queue_drain_mode", opt);
                await onRefresh();
              }}
              className={`rounded px-2.5 py-1 transition-colors cursor-pointer ${
                queueDrainMode === opt
                  ? "bg-brand-500 text-white"
                  : "text-text-secondary hover:bg-surface-700"
              }`}
            >
              {opt[0]!.toUpperCase() + opt.slice(1)}
            </button>
          ))}
        </div>
      </div>

      <CollapsibleSection
        title="Advanced"
        subtitle="Replay retention caps and daemon watchdog tuning. Touch only when triaging a specific failure mode."
      >
        <NumberField
          label="History cap (events)"
          description="Per-session retention cap on cockpit events. 0 = unlimited (default); set a non-zero value to bound disk usage on long-running sessions. Persists to config.toml as cockpit.replay_events; cross-device."
          value={
            typeof cockpit.replay_events === "number"
              ? (cockpit.replay_events as number)
              : 0
          }
          min={0}
          onChange={(v) => onSaveField("cockpit", "replay_events", v)}
        />
        <NumberField
          label="Replay buffer bytes"
          description="Per-session byte cap on the in-memory replay buffer. Persists to config.toml as cockpit.replay_bytes; cross-device."
          value={
            typeof cockpit.replay_bytes === "number"
              ? (cockpit.replay_bytes as number)
              : 0
          }
          min={0}
          onChange={(v) => onSaveField("cockpit", "replay_bytes", v)}
        />
        <NumberField
          label="Max concurrent resumes"
          description="Upper bound on parallel cockpit worker spawns/attaches the reconciler runs on `aoe serve` cold start. Default 4 keeps Node.js bootup memory bounded for laptops/Pis (each claude-agent-acp is ~50-80 MB transient). Bounded at runtime by `min(this, max_concurrent_workers).max(1)`. Persists to config.toml as cockpit.max_concurrent_resumes; cross-device."
          value={maxConcurrentResumes}
          min={1}
          onChange={(v) => onSaveField("cockpit", "max_concurrent_resumes", v)}
        />
        <NumberField
          label="Silent-orphan grace (s)"
          description="Daemon-side watchdog grace before declaring a prompt orphaned and restarting the worker. Fires when the agent finishes streaming but the adapter never sends PromptResponse (upstream agentclientprotocol/claude-agent-acp#688). Active only when no in-flight tool call is open and the prompt has produced at least one progress event, so long-running tools are unaffected. 0 disables. Default 60. Persists to config.toml as cockpit.silent_orphan_grace_secs; cross-device. See #1240."
          value={
            typeof cockpit.silent_orphan_grace_secs === "number"
              ? (cockpit.silent_orphan_grace_secs as number)
              : 60
          }
          min={0}
          onChange={(v) => onSaveField("cockpit", "silent_orphan_grace_secs", v)}
        />
        <NumberField
          label="Silent-orphan fast grace (s)"
          description="Accelerated silent-orphan grace, used once a cost-populated UsageUpdate has arrived for the current prompt (the claude-agent-acp wrap-up accounting marker emitted just before PromptResponse). Lowers MTTR on the known adapter wedge without weakening the vendor-agnostic baseline. 0 disables the accelerator (cost UsageUpdate stops reducing the effective grace). Default 20. Persists to config.toml as cockpit.silent_orphan_fast_grace_secs; cross-device. See #1240."
          value={
            typeof cockpit.silent_orphan_fast_grace_secs === "number"
              ? (cockpit.silent_orphan_fast_grace_secs as number)
              : 20
          }
          min={0}
          onChange={(v) => onSaveField("cockpit", "silent_orphan_fast_grace_secs", v)}
        />
        <NumberField
          label="Auto-stop idle workers (s)"
          description="Seconds of inactivity (no cockpit events, no in-flight turn) after which the daemon stops an idle cockpit worker and frees its resources. The session stays put; the next prompt respawns the worker seamlessly. 0 disables (default); no worker is ever stopped for inactivity. Checked about once a minute, so the stop can lag the threshold by up to a minute. Cockpit workers only. Persists to config.toml as cockpit.auto_stop_idle_secs; cross-device. See #1689."
          value={
            typeof cockpit.auto_stop_idle_secs === "number"
              ? (cockpit.auto_stop_idle_secs as number)
              : 0
          }
          min={0}
          onChange={(v) => onSaveField("cockpit", "auto_stop_idle_secs", v)}
        />
      </CollapsibleSection>

      {error && <div className="text-xs text-rose-400">{error}</div>}
    </div>
  );
}
