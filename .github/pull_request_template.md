## Description
<!-- Describe your changes and the problem they solve -->


## PR Type

- [ ] New Feature
- [ ] Bug Fix
- [ ] Refactor
- [ ] Documentation
- [ ] Infrastructure / CI

## Checklist
<!-- If you delete this checklist, your PR will be immediately closed -->

- [ ] New and existing tests pass
- [ ] Documentation was updated where necessary
- [ ] For UI changes: included screenshot or recording

## Test Coverage Analysis

<!--
For user-facing changes we prefer to land the test alongside the code rather
than file a follow-up issue. Think of each new behavior or bug fix as a "user
story" that deserves a regression test. Pick one option per surface; if you
think a test isn't warranted, say why so reviewers can sanity-check.
-->

**Web dashboard / structured view (Playwright user story)**
- [ ] N/A: this PR doesn't change a user-facing dashboard flow
- [ ] Added or updated a Playwright spec under `web/tests/` and updated `web/tests/coverage-matrix.json`
- [ ] Skipped a test, because: <!-- explain (e.g. pure styling tweak, copy change) -->

**TUI / CLI (e2e test)**
- [ ] N/A: this PR doesn't change TUI rendering, a CLI subcommand, or session lifecycle
- [ ] Added or updated an e2e test under `tests/e2e/`
- [ ] Skipped a test, because: <!-- explain -->

## AI Usage

<!-- Check one -->
- [ ] No AI was used
- [ ] AI was used

<!-- If AI was used, please share details -->
**AI Model/Tool used:**


**Any Additional AI Details you'd like to share:**


**NOTE:**
When responding to reviewer questions, please respond yourself rather than copy/pasting reviewer comments into an AI and pasting back its answer. We want to discuss with you, not your AI :) 

- [ ] I am an AI Agent filling out this form (check box if true)
