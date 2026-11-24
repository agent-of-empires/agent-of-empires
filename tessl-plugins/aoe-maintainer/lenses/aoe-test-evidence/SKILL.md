---
name: aoe-test-evidence
description: Review an Agent of Empires change for whether its tests prove the behavior they claim, that a regression test reaches the changed branch and would fail without the fix, that its outcome does not depend on scheduling or on state shared with other tests, and that it never touches the developer's real environment. Use as one lens in a code review run.
---

# Review lens: AoE test evidence

Review the tests a change adds or edits, and the tests its fix implies, for whether they would catch the regression they exist for.

## Scope

- **Reach** Whether each test added for a fix or a new branch drives that branch through the path production uses, and would fail on the code before the change. A test that starts from a state the bug cannot reach, stubs the step that broke, or asserts something true either way proves nothing. Browser-only behavior (focus, keyboard, hit-testing, clipping at a viewport width) asserted only under jsdom does not reach it; web/AGENTS.md puts it in mocked Playwright. An existing test the change leaves asserting the old rule belongs here.
- **Determinism** Whether the outcome depends on task scheduling, wall-clock margins, directory or hash iteration order, reading a file or socket before its writer finishes, or process-global state such as a static registry, counter, semaphore or coordinator. Tests must pass both under nextest, one process per test, and under `cargo test`, where tests in a binary run as threads of one process and share that state.
- **Isolation** Whether a test, fixture, benchmark or script reads or writes state outside its sandbox: the user's git config and hooks, the macOS Keychain, agent config roots, XDG directories, inherited `TMUX`, `AOE_TMUX_SOCKET` or SSH variables, or a daemon or port something else owns. `tests/e2e/harness.rs` and `web/tests/helpers/aoeServe.ts` isolate HOME, tmux sockets and ports; anything else is the test's to isolate.

## Method

Start from the production change, not the test file. For each fixed bug or new branch, name the input or interleaving that reaches it, find the test that supplies it, and check the assertion against the behavior before the change. For each added test, list what it reads from outside its own setup (environment variables, home-relative paths, global registries, counters, semaphores, shared servers) and whether another test or the developer's machine can change it. For each fixture, check which production sources it enables, such as a credential store, a container runtime or a real config path, where a test variant exists. When a wait or assertion depends on another task, find the event it waits for; a deadline bounds failure, and a fixed microtask flush or a count of advanced fake timers does not show the event happened.

## Threshold

Report when a test can pass while the behavior it names is broken, can fail on a correct build because of timing, ordering or parallel tests, or can read or modify the developer's real environment.

Do not report:
- Fixed sleeps in tests; the `no-fixed-sleeps-in-tests` verifier owns them.
- Missing `web/tests/coverage-matrix.json` entries; the `web-change-updates-coverage-matrix` verifier owns them. An entry or note that claims coverage its named test does not assert is in scope here.
- Rust test code that starts git, tmux or an aoe child, or reads an environment variable, without the isolation the `tests-isolated-from-host-state` verifier requires; report isolation here only when that verifier's rule does not cover it.
- Races in production code; `aoe-runtime-lifecycle` owns them. Report here only the test that fails to pin one.
- A general request for more tests, or tests for code the change does not alter.

## Reporting

- Name the test, the case it misses or the outside state it reads, and the build or interleaving under which it gives the wrong answer.
- State the fix: the input that reaches the branch, the barrier or observable predicate to wait on, the key to scope an assertion to, or the env guard or harness fixture to use.
