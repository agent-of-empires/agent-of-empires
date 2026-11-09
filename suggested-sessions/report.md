# Suggested sessions — QA report

- Date: 2026-11-09
- Build: debug binary of `feat/suggested-sessions` at `36884c4` (reports `1.22.3`, plugin API 21)
- Plugin: `agent-of-empires.github 2.4.0` from plugin-github#135, dev install
- Dashboard: the branch's own `web/` app, driven with Playwright
- Data: synthetic — twelve suggestions across `acme/widgets`, `acme/api` and `example-org/docs`, so nothing from a real account appears in these images

## 🧪 QA — PASS ✅

### 1. Home dashboard suggests sessions
![Home dashboard](001-home-dashboard-suggestions.png)

`/`. Observed: a `SUGGESTED SESSIONS` section with `View all (12)` and three rows, each with the project breadcrumb (`acme/widgets#412`), title, `GitHub · GitHub requested your review · acme-widgets`, and `Start` / `Dismiss`.

### 2. Full view lists every active item
![Suggestions view](002-suggestions-view-all.png)

`/suggestions`. Observed: `All 12`, provider chip `GitHub 12`, provider status `GitHub: updated just now`, org/repo chips with counts (`acme/ 9`, `acme/widgets 5`, `acme/api 4`, `example-org/docs 3`), rows grouped by project (`acme-widgets`, `acme-api`).

### 3. Source filter narrows the list
![Org filter](003-source-filter-org.png)

Clicked the `acme/` org chip. Observed: `Clear filters` appears and the list drops to the 9 items under that org.

### 4. Search narrows the list
![Search](004-search-pagination.png)

Typed `pagination` in `Search title, reference, reason or source`. Observed: 2 matching rows.

### 5. Dismiss and undo
![Dismiss](005-dismiss-undo-bar.png)

Clicked `Dismiss` on the first row. Observed: chips become `All 11` / `GitHub 11` / `Dismissed 1`, the row leaves the list, and an undo bar offers to restore it. Clicking `Undo` returned the list to `All 12`.

### 6. Settings › Suggestions
![Settings](006-settings-suggestions.png)

`/settings/suggestions`. Observed: homepage / sidebar / source-reference switches, the GitHub provider with nine per-criterion toggles (`Changes requested on your PRs`, `Review requests`, `Re-reviews`, `Review threads awaiting your reply`, `Merge conflicts on your PRs`, `Failing checks on your PRs`, `Mentions`, `Issues assigned to you`, `New issues`), the provider enable switch, and the repository / organization / ignore allowlists.

### 7. Turning a criterion off hides its items
![Criterion off](007-settings-source-off.png)
![View after criterion off](008-view-after-source-off.png)

Unticked `New issues` and opened `/suggestions`. Observed: provider chip drops from `GitHub 12` to `GitHub 10`, no refresh needed. Re-ticking restored `GitHub 12`.

### 8. Start opens the wizard prefilled
![Start wizard](009-start-wizard-prefill.png)

Clicked `Start` on the first row. Observed: the `New session` wizard opens with a GitHub source banner, the resolved project (`acme-widgets`), the title filled in, and the prompt rendered from the saved template with `From template: new_issues · Edit template`, plus `Launch session (⌘+Enter)`.


### 9. TUI — Home group, `S` view, dismiss
![TUI home suggested group](010-tui-home-suggested-group.png)
![TUI suggested sessions view](011-tui-suggested-view.png)
![TUI dismiss](012-tui-dismiss-item.png)

The TUI at 170x46 against the same profile. Home shows a `Suggested (12)` group; `Enter` expands it in place to the individual rows (`GitHub acme/widgets#412 Add …`). `S` opens the `Suggested sessions` view with project groups (`acme-widgets 5`), a selection marker, each row's source ref and reason, the provider status line (`GitHub: updated just now`), and the footer keymap (`j/k select · Enter review/start/open · N start another · x dismiss/restore · d dismissed · r refresh · l reload · PgUp/PgDn prompt · Esc back`). `x` on the selected row marks it `[dismissed, x to undo]`, and `x` again restores it. `Esc` returns to Home with the group still expanded.

**Could not verify:** pressing `Launch` (session creation and first-turn delivery), the TUI wizard's launch step, and read-only / CityHall refusal paths.


**Console:** no console errors or page errors in any step.

**Note:** these flows ran against mocked suggestion payloads served to the dashboard (the same approach `web/tests/dashboard-suggestions.spec.ts` takes); the provider, store and API were verified separately against a live account.
