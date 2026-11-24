// One SessionResponse shape for the mocked specs: pass only the fields a spec
// asserts on, and everything else takes the daemon's common defaults.

import type { Page } from "@playwright/test";

import { profileFor } from "../../src/lib/__tests__/agentProfileFixtures";

export interface SessionInput extends Record<string, unknown> {
  id: string;
  title?: string;
  project_path?: string;
}

export function sessionResponse({ id, title, project_path, ...rest }: SessionInput) {
  const projectPath = project_path ?? `/tmp/${id}`;
  const tool = typeof rest.tool === "string" ? rest.tool : "claude";
  return {
    id,
    title: title ?? id,
    project_path: projectPath,
    // In no Group, as the Daemon creates a session, so Repository shows no Group header of its own.
    group_path: "",
    tool,
    agent_profile: profileFor(tool),
    status: "Idle",
    yolo_mode: false,
    lifecycle_generation: 0,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
    ...rest,
  };
}

/** Route GET /api/sessions to a list; any other method is refused. */
export async function mockSessionsList(
  page: Page,
  sessions: () => SessionInput[],
  ordering: () => string[] = () => [],
) {
  await page.route(/\/api\/sessions(\?.*)?$/, (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
      json: { sessions: sessions().map(sessionResponse), workspace_ordering: ordering() },
    });
  });
}
