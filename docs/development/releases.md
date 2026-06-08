# Releases

How `agent-of-empires` ships. Maintainer-facing reference for the weekly automated cadence and the manual emergency-release path.

## Cadence

- **Baseline:** at least one release per week.
- A GitHub Action opens a release-staging PR every **Wednesday at 09:00 UTC** (see `.github/workflows/open-release-pr.yml`).
- Default semver bump is **patch**. The maintainer reviews the PR, optionally edits the version bump on the branch (patch -> minor / major), then clicks merge.
- Merging the PR fires `.github/workflows/tag-release-pr.yml`, which tags the merge commit. The tag push triggers `.github/workflows/release.yml`, which builds the four platform binaries and publishes the GitHub release + ClawHub artifact.

The staging PR body embeds a plain newest-first commit list for maintainer review only. The user-facing `CHANGELOG.md` and GitHub Release body are generated separately by [git-cliff](https://github.com/orhun/git-cliff), grouped by conventional-commit prefix; see [`cliff.toml`](https://github.com/agent-of-empires/agent-of-empires/blob/main/cliff.toml) and the "Changelog visibility" section of [`CONTRIBUTING.md`](https://github.com/agent-of-empires/agent-of-empires/blob/main/CONTRIBUTING.md). Folding the staging PR body into the same grouped render is queued under #1387.

The maintainer's only manual step on a normal release: review the PR, optionally edit the bump, merge.

## Weekly release-staging PR

The cron runs every Wednesday at 09:00 UTC. You can also trigger it manually:

```bash
gh workflow run open-release-pr.yml          # default: patch bump
gh workflow run open-release-pr.yml -f bump=minor
gh workflow run open-release-pr.yml -f bump=major
```

The workflow:

1. Reads the current version from `Cargo.toml`.
2. Computes the next version based on the bump (defaults to patch).
3. Refuses to run if the tag or the staging branch already exists.
4. Bumps `Cargo.toml` + `Cargo.lock` on a new branch `release-staging/vX.Y.Z` via `cargo set-version` + `cargo generate-lockfile`.
5. Regenerates `CHANGELOG.md` via `git cliff --tag vX.Y.Z` and stages it in the same release commit so the staging PR shows the changelog diff for review.
6. Dumps every commit since the last `v*` tag (newest first, `--no-merges`, author + short SHA + date) into the PR body as a maintainer-review summary.
7. Opens the PR labeled `release-staging`, with a `<!-- release-version: X.Y.Z -->` marker in the body.

### Adjusting the bump in-flight

If the auto-staged version is wrong (e.g., the diff contains a breaking change but the workflow picked patch), edit `Cargo.toml` + `Cargo.lock` on the staging branch and update the marker in the PR body to match. Re-run `git cliff --config cliff.toml --tag "vX.Y.Z" --output CHANGELOG.md` on the branch so the changelog header reflects the corrected version. The post-merge tagger reads the version from `Cargo.toml` at the merge commit and cross-checks it against the marker; if they disagree the tag step refuses to run.

## Post-merge tagging

`.github/workflows/tag-release-pr.yml` listens for merged PRs labeled `release-staging` on `main`. It:

1. Checks out `github.event.pull_request.merge_commit_sha`. This is the exact commit GitHub produced from the merge; it is immutable and anchored regardless of what lands on `main` afterwards.
2. Reads the version from `Cargo.toml`, sanity-checks `Cargo.lock` agrees, and verifies the PR body marker matches.
3. Refuses to tag if `vX.Y.Z` already exists.
4. Warns (does not fail) if `main` advanced after the merge.
5. Pushes an annotated `vX.Y.Z` tag pointing at `merge_commit_sha` using `RELEASE_TOKEN` so the tag push triggers downstream workflows (`GITHUB_TOKEN` does not).

The tag push fires `release.yml` exactly as it does today.

### Why we tag the merge SHA, not `main`

Between the moment the staging PR merges and the moment the tag push completes, other PRs can land on `main`. If we tagged `origin/main`, the tag would point at a commit whose `Cargo.toml` no longer matches the version, and `release.yml`'s validate step would fail. Tagging `merge_commit_sha` dissolves that race.

## Emergency releases

The original `.github/workflows/prepare-release.yml` is still wired up for emergencies. It is a manual `workflow_dispatch` that takes a version, bumps `Cargo.toml` + `Cargo.lock`, and pushes the tag directly to `main`. Use it when:

- A critical fix needs to ship before the next Wednesday.
- The weekly workflow is broken for some reason and you need to cut a release by hand.

```bash
gh workflow run prepare-release.yml -f version=1.7.2
```

This bypasses the staging PR. `prepare-release.yml` regenerates `CHANGELOG.md` from git-cliff and folds it into the same `chore: bump version` commit; `release.yml` builds the binaries and creates the GitHub Release with the per-version body that git-cliff emits via `--current --strip header`.

## Skill hubs

`aoe` ships a coding-agent management skill to two skill hubs. The skill sources live in `contrib/`, and `cargo xtask check-skill` (run in CI) validates that both reference real CLI commands and follow each hub's frontmatter rules.

- **ClawHub** (`contrib/openclaw-skill/`): published automatically by the `publish-clawhub` job in `release.yml` on every release, using `CLAWHUB_TOKEN`. ClawHub is a registry, so each release idempotently pushes the new version. The frontmatter must not carry a top-level `version:` field; ClawHub's `_meta.json` and the `--version` flag are the source of truth.

- **Hermes Agent Skills Hub** (`contrib/hermes-skill/`): published manually. Unlike ClawHub, the Hermes hub is GitHub-PR-based: `hermes skills publish` forks the hub repo, commits the skill snapshot, and opens a PR for a maintainer to merge. There is no auto-sync, so the in-repo copy is the source of truth and the hub copy drifts until you re-publish. The frontmatter must carry a top-level `version:` field. To publish (or push an update), bump the `version:` in `contrib/hermes-skill/SKILL.md`, then run:

  ```bash
  hermes skills publish contrib/hermes-skill --to github --repo NousResearch/hermes-agent
  ```

  This requires the `hermes` CLI and a GitHub token with fork permissions (`GITHUB_TOKEN` in `~/.hermes/.env` or `gh auth login`). Re-run it whenever the skill changes meaningfully; cadence is "on change," not "every release."

## Versioning

We follow semver, but the autopick is patch. Maintainer adjusts when:

- **Major:** breaking config changes (e.g., the `update_check_mode` cutover in #1140, even though we ship a migration), removed CLI subcommands, on-disk format breakage that needs maintainer attention beyond a migration.
- **Minor:** new user-visible features, new CLI subcommands, new config sections, anything covered by a `feat:` commit since the last release.
- **Patch:** bug fixes, refactors, docs, perf, internal CI / tests.

If you are uncertain, the safer call is the bigger bump.
