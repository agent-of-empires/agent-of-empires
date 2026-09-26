// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "./api";
import type { ServerAbout } from "./api";
import type { CreateSessionRequest, SettingsFieldDescriptor } from "./types";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const empty = (status = 200) => new Response("", { status });
const offline = () => fetchSpy.mockRejectedValueOnce(new Error("offline"));

function lastCall() {
  const [url, init] = fetchSpy.mock.calls.at(-1)!;
  return { url: String(url), init };
}

const bodyOf = (init: RequestInit | undefined) => JSON.parse(init!.body as string);

/** `["METHOD /url", call, expectations]`; a missing `body` asserts none was sent. */
type RequestCase = [string, () => Promise<unknown>, { body?: unknown; respond?: Response; result?: unknown }?];

const session = { id: "s1" };
const plugins = { plugins: [], load_errors: [] };
const hit = { session_id: "s1", seq: 3, kind: "agent", snippet: "hit", match_count: 2 };
const switched = { session_id: "s-1", agent: "codex", before_seq: 41, switch_seq: 42, status: "ok" };
const skill = {
  directory: "review",
  name: "review",
  description: "",
  provenance: { kind: "aoe-managed" },
  content: "",
};
const preview = { kind: "consent_required", dismissed: false, consent: { id: "p" } };

const requestCases: RequestCase[] = [
  ["GET /api/sessions", () => api.fetchSessions(), { respond: json(session), result: session }],
  [
    "GET /api/sessions/search?q=foo%20bar",
    () => api.searchConversations("foo bar"),
    { respond: json({ results: [hit] }), result: [hit] },
  ],
  [
    "GET /api/recent-projects",
    () => api.fetchRecentProjects(),
    { respond: json({ projects: [] }), result: { projects: [] } },
  ],
  [
    "PUT /api/workspace-ordering",
    () => api.updateWorkspaceOrdering(["a", "b"]),
    { body: { order: ["a", "b"] }, result: true },
  ],
  ["POST /api/sessions/s1/terminal?index=0", () => api.ensureTerminal("s1"), { result: true }],
  ["POST /api/sessions/s1/container-terminal?index=2", () => api.ensureTerminal("s1", 2, true)],
  ["DELETE /api/sessions/s1/terminal?index=2", () => api.killTerminal("s1", 2), { result: true }],
  [
    "GET /api/sessions/s1/diff/files",
    () => api.getSessionDiffFiles("s1"),
    { respond: json({ files: [] }), result: { files: [] } },
  ],
  ["GET /api/sessions/s1/diff/file?path=src%2Fa+b.ts", () => api.getSessionFileContents("s1", "src/a b.ts")],
  ["GET /api/sessions/s1/diff/file?path=a.ts&repo=myrepo", () => api.getSessionFileContents("s1", "a.ts", "myrepo")],
  ["GET /api/sessions/s1/file?path=a+b.ts", () => api.getSessionFile("s1", "a b.ts")],
  ["GET /api/settings", () => api.fetchSettings()],
  ["GET /api/settings?profile=my%20profile", () => api.fetchSettings("my profile")],
  ["PATCH /api/settings", () => api.updateSettings({ a: 1 }), { body: { a: 1 }, result: true }],
  ["PATCH /api/theme", () => api.updateTheme({ name: "dracula" }), { body: { name: "dracula" }, result: true }],
  ["GET /api/app-state/web-ui-state", () => api.getWebUiState(), { respond: json({ k: "v" }), result: { k: "v" } }],
  [
    "PATCH /api/app-state/web-ui-state",
    () => api.patchWebUiState({ keep: "1", drop: null }),
    { body: { keep: "1", drop: null }, result: true },
  ],
  [
    "GET /api/tips",
    () => api.fetchTips(),
    { respond: json({ enabled: true, tips: [] }), result: { enabled: true, tips: [] } },
  ],
  ["POST /api/app-state/tip-seen", () => api.markTipSeen("pin"), { body: { id: "pin" }, result: true }],
  ["POST /api/tips/show", () => api.setShowTips(false), { body: { enabled: false }, result: true }],
  [
    "GET /api/sandbox/volume-ignores-preview?path=%2Frepo%2Fa+b",
    () => api.fetchVolumeIgnoresPreview("/repo/a b"),
    { respond: json({ acknowledged: false, globs: [] }), result: { acknowledged: false, globs: [] } },
  ],
  [
    "GET /api/sandbox/volume-ignores-preview?path=%2Frepo&profile=work",
    () => api.fetchVolumeIgnoresPreview("/repo", "work"),
  ],
  [
    "POST /api/app-state/volume-ignores-globs-acknowledged",
    () => api.markVolumeIgnoresGlobsAcknowledged(),
    { result: true },
  ],
  ["POST /api/profiles", () => api.createProfile("work"), { body: { name: "work" }, result: true }],
  ["DELETE /api/profiles/my%20work", () => api.deleteProfile("my work"), { result: true }],
  [
    "PATCH /api/profiles/old/rename",
    () => api.renameProfile("old", "new"),
    { body: { new_name: "new" }, result: true },
  ],
  ["PATCH /api/default-profile", () => api.setDefaultProfile("work"), { body: { name: "work" }, result: true }],
  [
    "GET /api/profiles/my%20work/settings",
    () => api.getProfileSettings("my work"),
    { respond: json({ description: "x" }), result: { description: "x" } },
  ],
  ["GET /api/themes", () => api.fetchThemes(), { respond: json(["empire"]), result: ["empire"] }],
  ["GET /api/themes/My%20Theme", () => api.fetchResolvedTheme("My Theme")],
  ["GET /api/theme/current", () => api.fetchCurrentTheme()],
  ["GET /api/sounds", () => api.fetchSounds(), { respond: json(["chime.wav"]), result: ["chime.wav"] }],
  [
    "GET /api/about",
    () => api.fetchAbout(),
    { respond: json({ build_flavor: "debug" }), result: { build_flavor: "debug" } },
  ],
  [
    "GET /api/telemetry/status",
    () => api.fetchTelemetryStatus(),
    { respond: json({ enabled: true }), result: { enabled: true } },
  ],
  [
    "POST /api/telemetry/consent",
    () => api.setTelemetryConsent(true),
    { body: { enabled: true }, respond: json({ enabled: true }), result: { enabled: true } },
  ],
  ["GET /api/system/update-status", () => api.fetchUpdateStatus()],
  [
    "POST /api/app-state/dismiss-update",
    () => api.dismissUpdate("1.2.3"),
    { body: { version: "1.2.3" }, result: true },
  ],
  ["POST /api/app-state/web-tour-seen", () => api.markWebTourSeen(), { result: true }],
  ["GET /api/git/branches?path=%2Frepo", () => api.fetchBranches("/repo")],
  ["GET /api/git/branches?path=%2Frepo&include_remote=true", () => api.fetchBranches("/repo", true)],
  [
    "GET /api/git/is-repo?path=%2Fr",
    () => api.fetchIsGitRepo("/r"),
    { respond: json({ is_git_repo: false }), result: false },
  ],
  [
    "POST /api/git/clone",
    () => api.cloneRepo("u"),
    { body: { url: "u" }, respond: json({ path: "/c" }), result: { ok: true, path: "/c" } },
  ],
  [
    "POST /api/git/clone",
    () => api.cloneRepo("u", { destination: "/d", shallow: true, bare: true }),
    { body: { url: "u", destination: "/d", shallow: true, bare: true } },
  ],
  ["GET /api/sessions/weird%2Fid/acp/context-primer?before_seq=42", () => api.fetchContextPrimer("weird/id", 42)],
  [
    "GET /api/acp/agents",
    () => api.fetchAcpAgents(),
    { respond: json([{ name: "codex" }]), result: [{ name: "codex" }] },
  ],
  [
    "GET /api/acp/option-catalog",
    () => api.fetchAcpOptionCatalog(),
    { respond: json({ version: 2, agents: {} }), result: { version: 2, agents: {} } },
  ],
  [
    "POST /api/sessions/weird%2Fid/acp/switch-agent",
    () => api.switchAcpAgent("weird/id", "codex"),
    { body: { target: "codex" }, respond: json(switched), result: switched },
  ],
  [
    "POST /api/sessions/s-1/acp/switch-agent",
    () => api.switchAcpAgent("s-1", "codex", "opus-4.7"),
    { body: { target: "codex", model: "opus-4.7" } },
  ],
  [
    "POST /api/sessions/s-1/acp/switch-agent",
    () => api.switchAcpAgent("s-1", "claude", null, "manual"),
    { body: { target: "claude", reason: "manual" } },
  ],
  [
    "POST /api/sessions/s-1/acp/enable",
    () => api.acpEnable("s-1"),
    { respond: json({ view: "structured" }), result: { view: "structured" } },
  ],
  ["POST /api/sessions/a%2Fb/acp/disable", () => api.acpDisable("a/b")],
  [
    "POST /api/sessions/s1/queue",
    () =>
      api.enqueueServerPrompt("s1", {
        id: "q1",
        text: "hi",
        createdAt: "t0",
        attachments: [{ kind: "image", mimeType: "image/png", name: "a.png", dataB64: "AA" }],
      }),
    {
      body: {
        id: "q1",
        text: "hi",
        created_at: "t0",
        attachments: [{ kind: "image", mime_type: "image/png", data: "AA", name: "a.png" }],
      },
      respond: json({ id: "q1", seq: 3 }),
      result: { id: "q1", seq: 3 },
    },
  ],
  [
    "GET /api/sessions/s1/queue",
    () => api.listServerQueue("s1"),
    { respond: json([{ id: "a" }]), result: [{ id: "a" }] },
  ],
  [
    "PATCH /api/sessions/s1/queue/q1",
    () => api.editServerQueuedPrompt("s1", "q1", "edited"),
    { body: { text: "edited" }, result: true },
  ],
  ["DELETE /api/sessions/a%2Fb/queue/c%20d", () => api.removeServerQueuedPrompt("a/b", "c d"), { result: true }],
  ["DELETE /api/sessions/s1/queue", () => api.clearServerQueue("s1"), { result: true }],
  ["GET /api/devices", () => api.fetchDevices()],
  ["DELETE /api/login/sessions/sess%2F1", () => api.revokeDevice("sess/1"), { result: true }],
  ["POST /api/login/logout-all", () => api.signOutAllDevices(), { result: true }],
  ["GET /api/login/status", () => api.loginStatus(), { respond: json({ required: true }), result: { required: true } }],
  ["GET /api/login/status", () => api.verifyToken(), { result: true }],
  ["GET /api/agents", () => api.fetchAgents(), { respond: json([{ id: "claude" }]), result: [{ id: "claude" }] }],
  ["GET /api/profiles", () => api.fetchProfiles()],
  ["GET /api/filesystem/home", () => api.getHomePath(), { respond: json({ path: "/home/u" }), result: "/home/u" }],
  [
    "GET /api/filesystem/browse?path=%2Frepo",
    () => api.browseFilesystem("/repo"),
    { respond: json({ entries: [], has_more: true }), result: { entries: [], has_more: true, ok: true } },
  ],
  [
    "GET /api/filesystem/browse?path=%2Frepo&limit=50&filter=src&show_hidden=true",
    () => api.browseFilesystem("/repo", 50, "src", true),
  ],
  ["GET /api/groups", () => api.fetchGroups()],
  ["GET /api/projects", () => api.fetchProjects()],
  ["GET /api/projects?scope=profile", () => api.fetchProjects("profile")],
  ["GET /api/claude-sessions", () => api.listClaudeSessions()],
  [
    "GET /api/docker/status",
    () => api.fetchDockerStatus(),
    { respond: json({ available: true, runtime: "docker" }), result: { available: true, runtime: "docker" } },
  ],
  [
    "POST /api/projects",
    () => api.createProject({ path: "/p", name: "p", scope: "global" }),
    {
      body: { path: "/p", name: "p", scope: "global" },
      respond: json({ name: "p" }),
      result: { ok: true, project: { name: "p" } },
    },
  ],
  [
    "DELETE /api/projects/my%20proj?scope=profile",
    () => api.deleteProject("my proj", "profile"),
    { result: { ok: true } },
  ],
  [
    "PATCH /api/projects/p?scope=global",
    () => api.updateProject("p", "global", "develop"),
    {
      body: { default_base_branch: "develop" },
      respond: json({ name: "p" }),
      result: { ok: true, project: { name: "p" } },
    },
  ],
  [
    "PATCH /api/projects/p?scope=global",
    () => api.updateProject("p", "global", "develop", { worktree_enabled: true, smart_rename: null }),
    {
      body: { default_base_branch: "develop", overrides: { worktree_enabled: true, smart_rename: null } },
      respond: json({}),
    },
  ],
  [
    "PATCH /api/projects/a%20b?scope=profile",
    () => api.setProjectPinned("a b", "profile", true),
    { body: { pinned: true }, respond: json({ pinned: true }), result: { ok: true, project: { pinned: true } } },
  ],
  [
    "POST /api/sessions",
    () => api.createSession({ path: "/repo", tool: "claude", trust_hooks: true } as CreateSessionRequest),
    {
      body: { path: "/repo", tool: "claude", trust_hooks: true },
      respond: json(session, 201),
      result: { ok: true, session },
    },
  ],
  ["PATCH /api/sessions/s1", () => api.renameSession("s1", "T"), { body: { title: "T" }, result: { ok: true } }],
  [
    "POST /api/sessions/s1/smart-rename",
    () => api.smartRenameSession("s1"),
    { respond: empty(202), result: { ok: true } },
  ],
  ["POST /api/sessions/s1/summarize", () => api.summarizeSession("s1"), { respond: empty(202), result: { ok: true } }],
  [
    "PATCH /api/sessions/s1/worktree-name",
    () => api.setWorktreeName("s1", "feature", true),
    { body: { name: "feature", rename_branch: true }, result: { ok: true } },
  ],
  ["PATCH /api/sessions/a%2Fb/group", () => api.updateSessionGroup("a/b", ""), { body: { group: "" }, result: true }],
  ...(
    [
      ["off", false],
      ["all", true],
      ["default", null],
    ] as const
  ).map(([preset, value]): RequestCase => [
    "PATCH /api/sessions/s1/notifications",
    () => api.setSessionNotifications("s1", preset),
    { body: { notify_on_waiting: value, notify_on_idle: value, notify_on_error: value }, result: true },
  ]),
  [
    "PATCH /api/sessions/s1/diff-base",
    () => api.setSessionDiffBase("s1", "develop"),
    { body: { base_branch: "develop" }, respond: json(session), result: session },
  ],
  [
    "PATCH /api/sessions/s1/diff-base",
    () => api.setSessionDiffBase("s1", "main", "r"),
    { body: { base_branch: "main", repo: "r" } },
  ],
  [
    "PATCH /api/sessions/s1/pin",
    () => api.setSessionPin("s1", false),
    { body: { pinned: false }, respond: json(session), result: session },
  ],
  ["PATCH /api/sessions/s1/color", () => api.setSessionColor("s1", null), { body: { color: null } }],
  [
    "PATCH /api/sessions/s1/archive",
    () => api.setSessionArchive("s1", true),
    { body: { archived: true, kill_pane: true } },
  ],
  [
    "POST /api/sessions/s1/trash",
    () => api.trashSession("s1"),
    { body: { kill_pane: true }, respond: json(session), result: session },
  ],
  ["POST /api/sessions/s1/restore", () => api.restoreSession("s1"), { respond: json(session), result: session }],
  ["POST /api/sessions/s1/stop", () => api.stopSession("s1"), { respond: json(session), result: session }],
  ["POST /api/sessions/s1/start", () => api.startSession("s1"), { respond: json(session), result: session }],
  ["PATCH /api/sessions/s1/snooze", () => api.setSessionSnooze("s1", 60), { body: { minutes: 60 } }],
  ["PATCH /api/sessions/s1/unread", () => api.setSessionUnread("s1", true), { body: { unread: true } }],
  [
    "DELETE /api/workspaces",
    () => api.deleteWorkspace(["a", "b"], { delete_worktree: true }),
    {
      body: { session_ids: ["a", "b"], delete_worktree: true },
      respond: json({ deleted: ["a"], failed: [{ id: "b", error: "boom" }], messages: ["m"] }),
      result: { ok: true, messages: ["m"], deleted: ["a"], failed: [{ id: "b", error: "boom" }] },
    },
  ],
  [
    "POST /api/sessions/s1/projects",
    () => api.attachSessionProject("s1", "/r"),
    {
      body: { project: "/r", attach_existing_branch: false },
      respond: json({
        worker: "restart_failed",
        worker_message: "boom",
        attached: { name: "r", branch: "b", branch_created: false, moved_to: "/w" },
        warnings: ["w"],
      }),
      result: {
        ok: true,
        worker: "restart_failed",
        message: "boom",
        name: "r",
        branch: "b",
        branchCreated: false,
        movedTo: "/w",
        warnings: ["w"],
      },
    },
  ],
  ["GET /api/mcp/servers", () => api.fetchMcpServers()],
  ["GET /api/mcp/servers?agent=my%20agent", () => api.fetchMcpServers("my agent")],
  [
    "POST /api/mcp/servers/a%2Fb/resolve",
    () => api.resolveMcpConflict("a/b", "claude", "aoe", "fp"),
    { body: { agent: "claude", winner: "aoe", fingerprint: "fp" }, result: "applied" },
  ],
  [
    "POST /api/mcp/servers/fs/keep",
    () => api.keepMcpServer("fs", "claude"),
    { body: { agent: "claude" }, result: true },
  ],
  [
    "POST /api/mcp/servers/fs/drop",
    () => api.dropMcpServer("fs", "claude"),
    { body: { agent: "claude" }, result: true },
  ],
  [
    "GET /api/skills",
    () => api.fetchSkills(),
    { respond: json({ skills: [], roots: [] }), result: { skills: [], roots: [] } },
  ],
  [
    "GET /api/skills/claude%20user/review%2Fa",
    () => api.fetchSkill("claude user", "review/a"),
    { respond: json(skill), result: skill },
  ],
  [
    "POST /api/skills",
    () => api.createSkill("mine", "Mine"),
    {
      body: { directory: "mine", description: "Mine" },
      respond: json({ directory: "mine" }, 201),
      result: { ok: true, directory: "mine", status: 201 },
    },
  ],
  [
    "PUT /api/skills/mine",
    () => api.updateSkill("mine", "c"),
    { body: { content: "c" }, respond: json({}), result: { ok: true, status: 200 } },
  ],
  [
    "POST /api/skills/claude-user/review/adopt",
    () => api.adoptSkill("claude-user", "review", "adopted"),
    { body: { destination: "adopted" } },
  ],
  ["DELETE /api/skills/mine", () => api.deleteSkill("mine"), { result: { ok: true, status: 200 } }],
  [
    "POST /api/skills/sync",
    () => api.syncSkills(),
    { body: {}, respond: json({ outcomes: [] }), result: { ok: true, outcomes: [], status: 200 } },
  ],
  [
    "POST /api/skills/sync",
    () => api.syncSkills({ roots: ["r"], replace: ["x"], directories: ["d"] }),
    { body: { roots: ["r"], replace: ["x"], directories: ["d"] } },
  ],
  ["GET /api/plugins", () => api.fetchPlugins(), { respond: json(plugins), result: plugins }],
  ["GET /api/plugins/commands", () => api.fetchPluginCommands()],
  ["GET /api/plugins/ui-state", () => api.fetchPluginUiState()],
  [
    "POST /api/plugins/commands/plugin.a.b/invoke",
    () => api.invokePluginCommand("plugin.a.b", "s1"),
    { body: { session_id: "s1" }, respond: empty(202), result: true },
  ],
  [
    "POST /api/plugins/p/action",
    () => api.invokePluginAction("p", "m", "s1"),
    {
      body: { method: "m", params: {}, session_id: "s1" },
      respond: json({ baseline_revision: 3 }),
      result: { baselineRevision: 3 },
    },
  ],
  [
    "POST /api/plugins/p/action",
    () => api.invokePluginAction("p", "m"),
    { body: { method: "m", params: {}, session_id: null }, respond: json({}), result: { baselineRevision: null } },
  ],
  [
    "POST /api/plugins/acme.cron/settings/options/resolve",
    () => api.resolvePluginOptions("acme.cron", "acp_agents", ["x"]),
    {
      body: { source: "acp_agents", depends: ["x"] },
      respond: json({ options: [{ value: "v", label: "l" }] }),
      result: [{ value: "v", label: "l" }],
    },
  ],
  [
    "POST /api/plugins/acme%2Fweird%20id/enabled",
    () => api.setPluginEnabled("acme/weird id", false),
    { body: { enabled: false }, respond: json(plugins), result: { kind: "ok", data: plugins } },
  ],
  [
    "GET /api/plugins/updates",
    () => api.fetchPluginUpdates(),
    { respond: json({ updates: [] }), result: { kind: "ok", updates: [] } },
  ],
  [
    "GET /api/plugins/discover?q=foo",
    () => api.discoverPlugins(" foo "),
    { respond: json({ results: [] }), result: { kind: "ok", results: [] } },
  ],
  ["GET /api/plugins/discover", () => api.discoverPlugins("  ")],
  [
    "GET /api/plugins/details?source=gh%3Aa%2Fb",
    () => api.fetchPluginDetails("gh:a/b"),
    { respond: json({ source: "gh:a/b" }), result: { kind: "ok", detail: { source: "gh:a/b" } } },
  ],
  [
    "GET /api/plugins/acme.plugin/update/preview",
    () => api.previewPluginUpdate("acme.plugin"),
    { respond: json(preview), result: { kind: "ok", preview } },
  ],
  [
    "POST /api/plugins/install/preview",
    () => api.previewPluginInstall("gh:a/b"),
    {
      body: { source: "gh:a/b" },
      respond: json({ fingerprint: "f" }),
      result: { kind: "ok", consent: { fingerprint: "f" } },
    },
  ],
  [
    "POST /api/plugins/acme.plugin/update/apply",
    () => api.applyPluginUpdate("acme.plugin", "fp"),
    {
      body: { expected_fingerprint: "fp" },
      respond: json({ job_id: "job1" }, 202),
      result: { kind: "ok", jobId: "job1" },
    },
  ],
  [
    "POST /api/plugins/install",
    () => api.startPluginInstall("gh:a/b", "fp"),
    {
      body: { source: "gh:a/b", expected_fingerprint: "fp" },
      respond: json({ job_id: "j" }),
      result: { kind: "ok", jobId: "j" },
    },
  ],
  ["POST /api/plugins/p/uninstall", () => api.startPluginUninstall("p"), { body: {}, respond: json({ job_id: "j" }) }],
  [
    "GET /api/plugins/jobs/j%2F1?tail=200",
    () => api.fetchPluginJob("j/1"),
    { respond: json({ job: { id: "j" } }), result: { kind: "ok", job: { job: { id: "j" } } } },
  ],
  [
    "POST /api/plugins/p/update/dismiss",
    () => api.dismissPluginUpdate("p", "fp"),
    { body: { fingerprint: "fp" }, result: { kind: "ok" } },
  ],
];

describe("request shapes", () => {
  it.each(requestCases)("%s", async (route, call, { body, respond, result } = {}) => {
    fetchSpy.mockResolvedValueOnce(respond ?? empty());
    const out = await call();
    const last = lastCall();
    expect(`${last.init?.method ?? "GET"} ${last.url}`).toBe(route);
    if (body === undefined) expect(last.init?.body).toBeUndefined();
    else expect(bodyOf(last.init)).toEqual(body);
    if (result !== undefined) expect(out).toEqual(result);
  });
});

const failureCases: [string, () => Promise<unknown>, unknown][] = [
  ["fetchSessions", () => api.fetchSessions(), null],
  ["searchConversations", () => api.searchConversations("q"), []],
  ["updateWorkspaceOrdering", () => api.updateWorkspaceOrdering([]), false],
  ["ensureTerminal", () => api.ensureTerminal("s1"), false],
  ["getSessionFileContents", () => api.getSessionFileContents("s1", "a"), null],
  ["fetchThemes", () => api.fetchThemes(), []],
  ["fetchAcpAgents", () => api.fetchAcpAgents(), []],
  ["fetchAcpOptionCatalog", () => api.fetchAcpOptionCatalog(), { version: 1, agents: {} }],
  ["fetchIsGitRepo", () => api.fetchIsGitRepo("/r"), null],
  ["getHomePath", () => api.getHomePath(), null],
  ["browseFilesystem", () => api.browseFilesystem("/r"), { entries: [], has_more: false, ok: false }],
  ["fetchDockerStatus", () => api.fetchDockerStatus(), { available: false, runtime: null }],
  [
    "loginStatus",
    () => api.loginStatus(),
    { required: false, authenticated: true, elevated: true, elevated_until_secs: null },
  ],
  ["verifyToken", () => api.verifyToken(), false],
  ["enqueueServerPrompt", () => api.enqueueServerPrompt("s1", { id: "q", text: "t" }), null],
  ["listServerQueue", () => api.listServerQueue("s1"), []],
  ["renameSession", () => api.renameSession("s1", "x"), { ok: false }],
  ["smartRenameSession", () => api.smartRenameSession("s1"), { ok: false }],
  ["attachSessionProject", () => api.attachSessionProject("s1", "p"), { ok: false }],
  ["resolveMcpConflict", () => api.resolveMcpConflict("n", "a", "aoe", "fp"), "error"],
  ["invokePluginAction", () => api.invokePluginAction("p", "m"), null],
  ["resolvePluginOptions", () => api.resolvePluginOptions("p", "s", []), []],
  ["fetchSoundBlob", () => api.fetchSoundBlob("x.wav"), null],
];

describe("failure fallbacks", () => {
  it.each(failureCases)("%s on non-2xx and network failure", async (_name, call, fallback) => {
    fetchSpy.mockResolvedValueOnce(new Response("nope", { status: 500 }));
    expect(await call()).toEqual(fallback);
    offline();
    expect(await call()).toEqual(fallback);
  });
});

describe("ensureSession", () => {
  it.each([
    ["success", json({ status: "restarted" }), { ok: true, status: "restarted" }],
    [
      "success warning",
      json({ status: "restarted", message: "Started fresh after unavailable resume" }),
      { ok: true, status: "restarted", message: "Started fresh after unavailable resume" },
    ],
    [
      "server error",
      json({ error: "boom", message: "no good" }, 500),
      { ok: false, error: "boom", message: "no good" },
    ],
    ["empty error body", empty(502), { ok: false, message: "Server error (502)" }],
  ])("%s", async (_name, response, expected) => {
    fetchSpy.mockResolvedValueOnce(response);
    expect(await api.ensureSession("s1")).toEqual(expected);
    expect(lastCall()).toMatchObject({ url: "/api/sessions/s1/ensure", init: { method: "POST" } });
  });

  it("distinguishes an abort from a network failure", async () => {
    fetchSpy.mockRejectedValueOnce(Object.assign(new Error("x"), { name: "AbortError" }));
    expect(await api.ensureSession("s1")).toEqual({ ok: false, error: "aborted" });
    offline();
    expect(await api.ensureSession("s1")).toEqual({ ok: false, message: "offline" });
  });
});
describe("acpDisable", () => {
  it("returns the updated terminal view", async () => {
    fetchSpy.mockResolvedValueOnce(json({ session_id: "s-1", view: "terminal" }));
    expect(await api.acpDisable("s-1")).toEqual({
      ok: true,
      data: { session_id: "s-1", view: "terminal" },
    });
  });

  it("surfaces recovery guidance from a refusal", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response("Run set-session-id --store /path to pin the native store", { status: 409 }),
    );
    expect(await api.acpDisable("s-1")).toEqual({
      ok: false,
      message: "Run set-session-id --store /path to pin the native store",
    });
  });

  it("returns a message-less failure for an empty or unreachable response", async () => {
    fetchSpy.mockResolvedValueOnce(empty(500));
    expect(await api.acpDisable("s-1")).toEqual({ ok: false });
    offline();
    expect(await api.acpDisable("s-1")).toEqual({ ok: false });
  });
});

it("forwards abort signals", async () => {
  const { signal } = new AbortController();
  fetchSpy.mockResolvedValueOnce(json({ results: [] }));
  await api.searchConversations("q", signal);
  expect(lastCall().init?.signal).toBe(signal);
  fetchSpy.mockResolvedValueOnce(json({}));
  await api.fetchContextPrimer("s1", 1, signal);
  expect(lastCall().init?.signal).toBe(signal);
});

it("pasteImage uploads raw base64 and returns the host path", async () => {
  fetchSpy.mockResolvedValueOnce(json({ path: "/wt/img.png" }));
  const file = new File(["hi"], "img.png", { type: "image/png" });
  expect(await api.pasteImage("s1", file)).toBe("/wt/img.png");
  expect(lastCall().url).toBe("/api/sessions/s1/paste-image");
  expect(bodyOf(lastCall().init)).toEqual({ mime_type: "image/png", data: btoa("hi") });
  fetchSpy.mockResolvedValueOnce(json({}));
  expect(await api.pasteImage("s1", file)).toBeNull();
});

it("fetchSoundBlob returns the file as a Blob", async () => {
  fetchSpy.mockResolvedValueOnce(new Response("bytes"));
  expect(await (await api.fetchSoundBlob("my sound.wav"))!.text()).toBe("bytes");
  expect(lastCall().url).toBe("/api/sounds/file/my%20sound.wav");
});

describe("fetchCityHallBundle", () => {
  it("returns the TOML body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("schema_version = 1\n"));
    await expect(api.fetchCityHallBundle()).resolves.toBe("schema_version = 1\n");
    expect(fetchSpy).toHaveBeenCalledWith("/api/cityhall/bundle");
  });

  it.each([
    [json({ message: "disabled in CityHall client mode" }, 403), "disabled in CityHall client mode"],
    [new Response("<html>502</html>", { status: 502 }), "Export failed (HTTP 502)"],
  ])("throws the server message or status", async (response, message) => {
    fetchSpy.mockResolvedValueOnce(response);
    await expect(api.fetchCityHallBundle()).rejects.toThrow(message);
  });
});

describe("installAcpAgent", () => {
  it("POSTs and returns the parsed body", async () => {
    const body = { session_id: "s-1", package: "p", success: true, exit_code: 0, stdout: "", stderr: "" };
    fetchSpy.mockResolvedValueOnce(json(body));
    expect(await api.installAcpAgent("weird/id")).toEqual(body);
    expect(lastCall()).toMatchObject({ url: "/api/sessions/weird%2Fid/acp/install-agent", init: { method: "POST" } });
  });

  it.each([
    ["server message", json({ error: "install_disabled", message: "Installing is off." }, 403), "Installing is off."],
    ["error code", json({ error: "install_disabled" }, 403), "install_disabled"],
    ["status fallback", new Response("boom", { status: 500 }), "Server returned 500"],
    ["invalid 2xx body", new Response("not json"), "invalid or empty response"],
  ])("throws on %s", async (_name, response, message) => {
    fetchSpy.mockResolvedValueOnce(response);
    await expect(api.installAcpAgent("s-1")).rejects.toThrow(message);
  });
});

describe("project mutations", () => {
  const calls: [string, () => Promise<{ ok: boolean; error?: string }>][] = [
    ["createProject", () => api.createProject({ path: "/p" })],
  ];

  it.each(calls)("%s maps JSON, text, and network errors", async (_name, call) => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "dup" }), { status: 409 }));
    expect(await call()).toEqual({ ok: false, error: "dup" });
    fetchSpy.mockResolvedValueOnce(new Response("boom", { status: 500 }));
    expect(await call()).toEqual({ ok: false, error: "boom" });
    fetchSpy.mockResolvedValueOnce(empty(500));
    expect(await call()).toEqual({ ok: false, error: "Server error (500)" });
    offline();
    expect(await call()).toEqual({ ok: false, error: "offline" });
  });
});

describe("createSession errors", () => {
  const body = { path: "/repo", tool: "claude" } as CreateSessionRequest;

  it.each([
    [
      "full hooks trust",
      {
        error: "hooks_need_trust",
        message: "trust me",
        on_create: ["a"],
        on_launch: ["b"],
        on_destroy: ["c"],
        needs_mcp_trust: true,
      },
      { onCreate: ["a"], onLaunch: ["b"], onDestroy: ["c"], needsMcpTrust: true },
    ],
    [
      "hooks trust defaults",
      { error: "hooks_need_trust", message: "trust me" },
      { onCreate: [], onLaunch: [], onDestroy: [], needsMcpTrust: false },
    ],
  ])("surfaces %s", async (_name, payload, hooksNeedTrust) => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify(payload), { status: 403 }));
    expect(await api.createSession(body)).toEqual({ ok: false, error: "trust me", hooksNeedTrust });
  });

  it("maps plain JSON, text, and network errors", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ error: "create_failed", message: "nope" }), { status: 400 }),
    );
    expect(await api.createSession(body)).toEqual({ ok: false, error: "nope" });
    fetchSpy.mockResolvedValueOnce(new Response("boom", { status: 500 }));
    expect(await api.createSession(body)).toEqual({ ok: false, error: "Server error (500): boom" });
    offline();
    expect(await api.createSession(body)).toEqual({ ok: false, error: "Network error: offline" });
  });
});

it("cloneRepo maps server and network errors", async () => {
  fetchSpy.mockResolvedValueOnce(json({ message: "no repo" }, 404));
  expect(await api.cloneRepo("u")).toEqual({ ok: false, error: "no repo" });
  fetchSpy.mockResolvedValueOnce(empty(500));
  expect(await api.cloneRepo("u")).toEqual({ ok: false, error: "Clone failed (500)" });
  offline();
  expect(await api.cloneRepo("u")).toEqual({ ok: false, error: "Network error: offline" });
});

describe("login", () => {
  it("login sends the passphrase with a device-binding secret", async () => {
    fetchSpy.mockResolvedValueOnce(empty());
    expect(await api.login("hunter2")).toEqual({ ok: true });
    const { url, init } = lastCall();
    expect(url).toBe("/api/login");
    const sent = bodyOf(init);
    expect(sent.passphrase).toBe("hunter2");
    expect(sent.device_binding_secret).toMatch(/.+/);
  });

  it("elevateLogin sends the binding header and returns the window", async () => {
    fetchSpy.mockResolvedValueOnce(json({ elevated_until_secs: 900 }));
    expect(await api.elevateLogin("hunter2")).toEqual({ ok: true, elevated_until_secs: 900 });
    const { url, init } = lastCall();
    expect(url).toBe("/api/login/elevate");
    expect((init?.headers as Record<string, string>)["X-Aoe-Device-Binding"]).toBeTruthy();
    expect(bodyOf(init)).toEqual({ passphrase: "hunter2" });
  });

  it.each([
    ["login", api.login, "Login failed (401)"],
    ["elevateLogin", api.elevateLogin, "Elevation failed (401)"],
  ] as const)("%s maps server and network errors", async (_name, call, statusMessage) => {
    fetchSpy.mockResolvedValueOnce(json({ message: "wrong" }, 401));
    expect(await call("bad")).toEqual({ ok: false, error: "wrong" });
    fetchSpy.mockResolvedValueOnce(empty(401));
    expect(await call("bad")).toEqual({ ok: false, error: statusMessage });
    offline();
    expect(await call("x")).toEqual({ ok: false, error: "Network error" });
  });

  it("logout POSTs and resolves even when the request fails", async () => {
    fetchSpy.mockResolvedValueOnce(empty());
    await api.logout();
    expect(lastCall()).toMatchObject({ url: "/api/logout", init: { method: "POST" } });
    offline();
    await expect(api.logout()).resolves.toBeUndefined();
  });
});

describe("session mutation messages", () => {
  it("renameSession keeps only string warnings", async () => {
    fetchSpy.mockResolvedValueOnce(json({ warnings: ["kept", 3, null] }));
    expect(await api.renameSession("s1", "T")).toEqual({ ok: true, warnings: ["kept"] });
  });

  it.each([
    ["renameSession", () => api.renameSession("s1", "x")],
    ["smartRenameSession", () => api.smartRenameSession("s1")],
    ["attachSessionProject", () => api.attachSessionProject("s1", "p")],
  ])("%s surfaces the server message", async (_name, call) => {
    fetchSpy.mockResolvedValueOnce(json({ message: "running" }, 409));
    expect(await call()).toEqual({ ok: false, message: "running" });
  });
});

describe("deleteWorkspace errors", () => {
  it("treats a 2xx without a deleted array as unconfirmed", async () => {
    fetchSpy.mockResolvedValueOnce(json({ status: "ok" }));
    expect(await api.deleteWorkspace(["a"])).toEqual({
      ok: false,
      error: "Server did not confirm which sessions were deleted",
    });
  });

  it("maps server and network errors", async () => {
    const failed = [{ id: "a", error: "dirty" }];
    fetchSpy.mockResolvedValueOnce(json({ message: "dirty", failed }, 500));
    expect(await api.deleteWorkspace(["a"])).toEqual({ ok: false, error: "dirty", failed });
    fetchSpy.mockResolvedValueOnce(empty(500));
    expect(await api.deleteWorkspace(["a"])).toEqual({ ok: false, error: "Server error (500)" });
    offline();
    expect(await api.deleteWorkspace(["a"])).toEqual({ ok: false, error: "Network error: offline" });
  });
});

it("resolveMcpConflict maps 409 to stale", async () => {
  fetchSpy.mockResolvedValueOnce(empty(409));
  expect(await api.resolveMcpConflict("n", "a", "native", "fp")).toBe("stale");
});

it("skill mutations keep the status and message", async () => {
  fetchSpy.mockResolvedValueOnce(json({ message: "already exists" }, 409));
  expect(await api.createSkill("mine")).toEqual({ ok: false, error: "already exists", status: 409 });
  offline();
  expect(await api.deleteSkill("mine")).toEqual({ ok: false, error: "Network error: offline" });
  fetchSpy.mockResolvedValueOnce(json({ message: "read only" }, 403));
  expect(await api.syncSkills()).toEqual({ ok: false, outcomes: [], error: "read only", status: 403 });
  fetchSpy.mockResolvedValueOnce(empty(500));
  expect(await api.syncSkills()).toEqual({ ok: false, outcomes: [], error: "Server error (500)", status: 500 });
  offline();
  expect(await api.syncSkills()).toEqual({ ok: false, outcomes: [], error: "Network error: offline" });
});

describe("plugin results", () => {
  const cases: [string, () => Promise<unknown>, string, unknown][] = [
    ["fetchPluginUpdates", () => api.fetchPluginUpdates(), "Update check failed (HTTP 500).", {}],
    ["discoverPlugins", () => api.discoverPlugins("q"), "Discovery failed (HTTP 500).", {}],
    ["fetchPluginDetails", () => api.fetchPluginDetails("s"), "Details failed (HTTP 500).", {}],
    ["previewPluginUpdate", () => api.previewPluginUpdate("p"), "Update preview failed (HTTP 500).", { kind: "bogus" }],
    ["previewPluginInstall", () => api.previewPluginInstall("s"), "Install preview failed (HTTP 500).", {}],
    ["applyPluginUpdate", () => api.applyPluginUpdate("p", null), "Request failed (HTTP 500).", { nope: true }],
    ["setPluginEnabled", () => api.setPluginEnabled("p", true), "Failed to enable plugin (500).", { nope: true }],
    ["dismissPluginUpdate", () => api.dismissPluginUpdate("p", "f"), "Dismiss failed (HTTP 500).", undefined],
  ];

  it.each(cases)("%s maps failures to an error message", async (_name, call, statusMessage, malformed) => {
    fetchSpy.mockResolvedValueOnce(json({ message: "boom" }, 400));
    expect(await call()).toEqual({ kind: "error", message: "boom" });
    fetchSpy.mockResolvedValueOnce(new Response("not json", { status: 500 }));
    expect(await call()).toEqual({ kind: "error", message: statusMessage });
    offline();
    expect(await call()).toEqual({ kind: "error", message: "Network error." });
    if (malformed !== undefined) {
      fetchSpy.mockResolvedValueOnce(json(malformed));
      expect(await call()).toMatchObject({ kind: "error" });
    }
  });

  it("previewPluginUpdate rejects payloads missing per-kind fields", async () => {
    for (const bad of [
      { kind: "safe_update", to_version: "2" },
      { kind: "consent_required", dismissed: false },
    ]) {
      fetchSpy.mockResolvedValueOnce(json(bad));
      expect((await api.previewPluginUpdate("p")).kind).toBe("error");
    }
    const noUpdate = { kind: "no_update" };
    fetchSpy.mockResolvedValueOnce(json(noUpdate));
    expect(await api.previewPluginUpdate("p")).toEqual({ kind: "ok", preview: noUpdate });
  });

  it("fetchPluginJob keeps the HTTP status on failure", async () => {
    fetchSpy.mockResolvedValueOnce(json({ message: "gone" }, 404));
    expect(await api.fetchPluginJob("j")).toEqual({ kind: "error", status: 404, message: "gone" });
    fetchSpy.mockResolvedValueOnce(json({}, 200));
    expect(await api.fetchPluginJob("j")).toEqual({
      kind: "error",
      status: 200,
      message: "Job status failed (HTTP 200).",
    });
    offline();
    expect(await api.fetchPluginJob("j")).toEqual({ kind: "error", status: 0, message: "Network error." });
  });
});

describe("telemetry pings", () => {
  it.each([
    ["reportTelemetrySeen", () => api.reportTelemetrySeen("web"), "/api/telemetry/seen", { surface: "web" }],
    [
      "reportAcpInteraction",
      () => api.reportAcpInteraction("prompt_queued"),
      "/api/telemetry/structured-interaction",
      { kind: "prompt_queued" },
    ],
  ])("%s POSTs fire-and-forget and swallows failures", (_name, call, url, body) => {
    fetchSpy.mockResolvedValueOnce(empty());
    call();
    expect(lastCall()).toMatchObject({ url, init: { method: "POST" } });
    expect(bodyOf(lastCall().init)).toMatchObject(body);
    offline();
    expect(call).not.toThrow();
  });

  it("reportTelemetrySeen includes the form factor", () => {
    fetchSpy.mockResolvedValueOnce(empty());
    api.reportTelemetrySeen("diff_panel");
    expect(bodyOf(lastCall().init)).toHaveProperty("form_factor");
  });
});

it.each([
  [{ build_flavor: "debug" }, true],
  [{ build_flavor: "release" }, false],
  [null, false],
  [undefined, false],
])("isDebugBuild(%o) is %s", (about, expected) => {
  expect(api.isDebugBuild(about as ServerAbout | null | undefined)).toBe(expected);
});

describe("profile settings write guard", () => {
  const field = (section: string): SettingsFieldDescriptor => ({
    section,
    field: "f",
    category: "Test",
    label: "f",
    description: "",
    widget: { kind: "toggle" },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
  });
  const schema = [field("theme"), field("session")];

  beforeEach(() => api.resetSettingsSchemaCache());

  it("derives writable sections from the schema plus description", () => {
    const writable = api.profileWritableSections(schema);
    expect([...writable].sort()).toEqual(["description", "session", "theme"]);
  });

  it("caches a successful schema fetch and retries a failed one", async () => {
    fetchSpy.mockResolvedValueOnce(empty(503));
    expect(await api.getSettingsSchema()).toBeNull();
    fetchSpy.mockResolvedValueOnce(json(schema));
    expect(await api.getSettingsSchema()).toEqual(schema);
    expect(await api.getSettingsSchema()).toEqual(schema);
    expect(fetchSpy).toHaveBeenCalledTimes(2);
    expect(lastCall().url).toBe("/api/settings/schema");
  });

  it.each([
    ["hooks", { hooks: { on_create: ["rm -rf /"] } }],
    ["a blocked key beside an allowed one", { theme: { name: "empire" }, custom_agents: { evil: "x" } }],
  ])("refuses %s without sending", async (_name, updates) => {
    fetchSpy.mockResolvedValueOnce(json(schema));
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    expect(await api.updateProfileSettings("work", updates)).toBe(false);
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("PATCHes an allowed section", async () => {
    fetchSpy.mockResolvedValueOnce(json(schema)).mockResolvedValueOnce(empty());
    expect(await api.updateProfileSettings("work", { description: "mine" })).toBe(true);
    expect(lastCall()).toMatchObject({ url: "/api/profiles/work/settings", init: { method: "PATCH" } });
    expect(bodyOf(lastCall().init)).toEqual({ description: "mine" });
  });

  it("defers to the server when the schema is unavailable", async () => {
    fetchSpy.mockResolvedValueOnce(empty(503)).mockResolvedValueOnce(empty());
    expect(await api.updateProfileSettings("work", { hooks: {} })).toBe(true);
    expect(fetchSpy).toHaveBeenCalledTimes(2);
    expect(lastCall().url).toBe("/api/profiles/work/settings");
  });
});
