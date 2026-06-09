# Live Playwright Suite Audit (2026-06-09)

Goal: shrink the live suite (180 spec files, ~238 tests, one `aoe serve` +
tmux + isolated HOME per test) by moving specs that don't need a real backend
to the mocked Playwright suite or Vitest, per the placement rules in
AGENTS.md. Branch: `feature/playwright-live-speedup`.

Method: six parallel readers classified every spec by what its assertions
actually exercise; gaps re-audited by hand; verdicts below include a
judgment pass that overrode raw classifications where the reasoning was weak
(noted inline). Baseline job cost: 502s total, 350s test phase.

Templates that make demotion practical today:

- Mocked ACP frame replay: `web/tests/acp-edit-card-diff.spec.ts`
  (`page.routeWebSocket(/\/acp\/ws/)` + canned frames, no daemon).
- Vitest payload tests: `web/src/components/settings/__tests__/SoundSettings.test.tsx`.

Counts: KEEP-LIVE ~104 | DEMOTE-MOCKED ~55 | DEMOTE-VITEST ~13 | DELETE ~8.
~76 specs (~42%) leave the live suite. Estimated live test phase after:
~230s (from 350s); after 2-way sharding: ~115s/shard.

## DELETE (dead or duplicate)

- acp-stories/auth-sign-out.spec.ts — generic login/logout flow, duplicated by the auth-\* root specs.
- acp-stories/composer-mobile-enter-newline.spec.ts — permanently skipped (Playwright device-emulation limitation, #1383); dead weight.
- acp-stories/profile-create.spec.ts, profile-delete.spec.ts, profile-rename.spec.ts, profile-switch-view.spec.ts — duplicate root profile-lifecycle coverage; consolidate into the demoted mocked profile-lifecycle spec.
- acp-stories/wizard-launch-cmd-enter.spec.ts — near-duplicate of wizard-launch-button (keyboard variant of the same POST); the Cmd+Enter binding itself moves to a mocked assertion.
- acp-stories/diff-hunk-comment.spec.ts — overlaps mocked `diff-comments.spec.ts`, which already exercises the comment form against a stubbed WS.

## DEMOTE TO MOCKED PLAYWRIGHT (~55)

Pure UI behavior on stubbable data (`page.route()` + canned JSON, plus
`routeWebSocket` frame replay where the input is an ACP stream).

Keyboard / modal / navigation (template: plain page.route):

- acp-stories: shortcut-toggle-diff, shortcut-toggle-right-panel, shortcut-toggle-sidebar, shortcut-escape-closes-palette, shortcut-help-key, shortcut-new-session-key, shortcut-palette, shortcut-settings-key, modal-about, modal-about-escape, modal-help, topbar-go-to-dashboard, sidebar-row-click-navigate, projects-open, wizard-close-via-escape, wizard-open-close
- root: palette-scratch-launch

Sidebar state (localStorage-driven, canned session lists):

- root: sidebar-group-reorder, sidebar-groups, sidebar-groups-axis, sidebar-nested-axis, sidebar-sort-mode
- acp-stories: sidebar-filter, sidebar-fold-group, sidebar-resize-persist, sidebar-topbar-toggle, sidebar-reorder-reload-persists (needs a stateful workspace-ordering stub)

Wizard form UI (no launch round-trip asserted):

- acp-stories: wizard-branch-from-title, wizard-browse-disk, wizard-clone-url-input, wizard-creating-banner, wizard-extra-repos, wizard-group-field, wizard-last-tool-persists, wizard-prefill-from-group, wizard-recent-project, wizard-session-title-edit, wizard-tabs-visible, wizard-worktree-toggle
- root: wizard-scratch-enables-next, wizard-scratch-grouping, wizard-scratch-hides-worktree, wizard-scratch-toggle-visible, wizard-scratch-shortcut (prefill UI; launch path stays covered by wizard-scratch-launch)

ACP stream rendering (template: acp-edit-card-diff routeWebSocket replay):

- acp-stories: chat-bubble-overflow, composer-mobile-footer-actions, composer-send-enter, composer-single-newline-renders, composer-streamed-response, edit-card-diff-scroll, fold-failed-tool-card
- root: acp-config-pickers-ui (round-trip stays covered by acp-config-pickers), acp-telemetry-seen

Profiles (consolidated; CRUD forms on stubbed endpoints):

- root: profile-lifecycle, profiles-page
- (profile-override KEPT live; see notes)

Misc:

- root: theme-onboarding (server flag stubbable; theme paint covered by theme-switch-live)
- root: settings-advanced-fold (fold/visibility UI; the nested-field PATCH payloads move to Vitest)

## DEMOTE TO VITEST (~13)

Payload-shape and component-state tests (SoundSettings.test.tsx pattern):

- acp-stories: composer-draft-reload, composer-draft-session-switch, composer-ios-dictation, composer-toolbar-insert, delete-session-with-worktree (checkbox -> DELETE payload), diff-base-picker, mobile-toolbar-ctrl-modifier, mobile-toolbar-hidden-on-desktop, settings-logging-level, settings-sound-toggle (duplicates the canonical Vitest example), settings-tmux-mouse, settings-tmux-select, settings-update-interval, settings-theme-color-mode
- root: wizard-resolved-launch-command
- acp-stories: delete-active-session, diff-viewer-shows-files — either Vitest or mocked; decide at implementation.

## KEEP LIVE (~104)

Everything else. The load-bearing categories, each needing the real daemon:

- Auth: auth-login-passphrase, auth-no-auth, auth-token-entry, auth-token-rotation, login-session-persistence, devices
- Persistence/daemon lifecycle: settings-persistence-theme (canonical: disk + daemon-restart survival), settings-persistence-schema (generic schema path), settings-persistence-theme-passphrase (elevation), settings-persistence-acp (server-side node_path stripping is a security assertion), disconnect-banner, ensure-session-restart, file-watch-peer-propagation, tutorial, theme-switch-live, settings-theme-custom/-malformed/-failure-revert/-select, golden-path, workspace-ordering, group-collapse via triage-\*, sidebar-bulk-triage, triage-archive/pin/snooze, session-delete/rename/group-edit, telemetry-form-factor, feature-usage-signals, theme/profile isolation (profile-settings-isolation, profile-override), projects-add/edit/remove, mcp-servers, read-only-mode
- Git: git-clone, multi-repo-stale-base, session-create-duplicate-worktree, diff-base-from-worktree, right-panel-diff-files/-large-diff/-notifications, acp-file-links, wizard-scratch-launch, wizard-scratch-delete-cleans-tempdir, wizard-acp-create-session, wizard-launch-button, directory-browser
- tmux/PTY: terminal-copy-select, mobile-toolbar-arrow-up/ctrl-c/escape/tab, shortcut-terminal-focus, sidebar-select-focuses-terminal
- ACP protocol round-trips: all root acp-_ not listed above (approval, attachment, config-pickers, context-primer, custom-agent, disable, files, force-end-turn, mode-switch, replay-catchup, session-model, spawn-prompt, view-switch, worker-log) plus acp-stories approval-allow/deny, acp-mode-picker, acp-plan-strip, composer-queue-follow-up, composer-slash-pick-no-arg, composer-stop-_ (4), queue-\* (4), sidebar-plan-progress, sidebar-queued-count, sidebar-rate-limit, sidebar-select-focuses-composer, startup-error-banner-native-binary, stop-tool-card-terminal, view-switch-to-acp, palette-session-switch
- Skipped placeholders blocked on #1237 (zero CI cost, keep as-is): acp-cancel, acp-escape-no-cancel, acp-lagged-ws

## Judgment overrides applied to raw agent verdicts

1. Per-field settings "persistence across reload" specs were flagged
   DEMOTE-MOCKED with the reasoning "daemon call is mocked" — wrong frame:
   persistence against a stub proves nothing. The correct frame is
   consolidation: settings are schema-driven through one generic merge path,
   so ONE canonical live persistence spec (settings-persistence-theme +
   settings-persistence-schema) covers the pipeline and per-field variants
   become Vitest payload tests.
2. settings-persistence-acp kept live: the node_path stripping it asserts is
   a server-side RCE-surface control.
3. profile-override kept live (server-side sparse-merge isolation);
   acp-stories profile CRUD specs deleted as duplicates instead.
4. delete-active-session / delete-session-with-worktree raw verdicts said
   Vitest; the DELETE round-trip half is already covered live by
   session-delete, so only the payload/UI half moves.

## Implementation phases

1. Deletes + keyboard/modal/sidebar mocked demotions (no new harness needed).
2. Wizard + profiles + settings consolidation (mocked + Vitest).
3. ACP rendering demotions via routeWebSocket frame replay.
4. (Separate PR) shard the remaining live suite 2-way on free ubuntu runners.

Every phase must update web/tests/coverage-matrix.json (CI enforces matrix
entries) and keep codecov patch coverage intact.
