# Releases

Maintainer reference for how `agent-of-empires` ships: stable releases cut from a release PR, plus a daily nightly channel.

## Stable releases

`.github/workflows/release-plz.yml` runs [release-plz](https://release-plz.dev) on every push to `main`, daily, and on `workflow_dispatch`. It keeps one release PR open, branch `release-plz-*` and label `release`, that bumps `Cargo.toml` and `Cargo.lock` and prepends the new section to `CHANGELOG.md`. Each refresh force-pushes the PR and dismisses its approval, so approve it right before queueing. CI skips release PRs on `pull_request`; the merge queue runs the full suite on them. When it merges, release-plz pushes the `vX.Y.Z` tag on the PR's own squash commit with `RELEASE_TOKEN`, and the tag push starts `release.yml`. That workflow waits for `release` environment approval, builds the platform binaries through `build-binaries.yml`, publishes the GitHub release with the version's `CHANGELOG.md` section as its body, and publishes the ClawHub skill. `docker.yml` pushes the sandbox images for the same tag. If the `Tag release` job warns, the commits it lists merged after the PR's last refresh: they ship in the tag but are missing from `CHANGELOG.md`. Add the ones that need an entry (types `release-plz.toml` does not skip, and breaking commits) to that version's section in a follow-up PR and to the GitHub release notes.

In the merge queue, CI's `Release PR Up To Date` job fails the release PR if commits that need a changelog entry landed after its last refresh. The push that landed them refreshes the release PR (or dispatch `release-plz.yml`); re-approve it, then queue it again. The job blocks only while it is a required status check in the `main` ruleset.

Until #5504 moves it to a GitHub App, `RELEASE_TOKEN` is a personal access token of @njbrake, so the release PR is authored by him and must be approved by another member of `@agent-of-empires/protected`, since it changes `Cargo.toml` and `Cargo.lock`.

Configuration lives in `release-plz.toml`: only `agent-of-empires` is tagged, versions come from `v*` tags, and nothing is published to crates.io. The workspace crates share its version, so a change to `aoe-plugin-api`, `aoe-settings-derive` or `xtask` alone still bumps the release and appears in its changelog. The changelog groups commits by conventional-commit type; `pr-title-check.yml` keeps PR titles, which become squash-commit subjects, within those types.

release-plz force-pushes the release PR on every update, so edits made on its branch are lost. To ship a different version, merge a normal PR that sets the same version in all four workspace `Cargo.toml` files (the root, `aoe-plugin-api`, `aoe-settings-derive` and `xtask`) and in `Cargo.lock`. The version group takes the highest proposed version, so a root-only override loses to child commits. release-plz keeps a version that is already ahead of the last tag.

## Rebuilding an existing tag

Dispatch the release workflow with the existing tag, subject to the same release-environment approval:

```bash
gh workflow run release.yml -f tag=v1.18.0
```

The build checks out the requested tag and reads `[toolchain].channel` from its `rust-toolchain.toml`. Numeric pins are preserved. Historical tags using `stable` install the current stable compiler through the external setup action, without requiring helper files in the old tree. Such rebuilds do not reproduce the original compiler version.

## Versioning

Semver, from conventional commits since the last `v*` tag: `feat` is minor, anything else is patch, and any `!` (`feat!:`, `fix!:`) or a `BREAKING CHANGE:` footer is major. Mark a commit `!` only when it warrants a major release. Bump minor for user-visible features without a `feat` prefix, new CLI subcommands, or new config sections. Bump major for breaking config changes, removed CLI subcommands, or on-disk breakage needing maintainer attention beyond a migration. Override the computed bump as described in [Stable releases](#stable-releases). When uncertain, take the bigger bump.

## Nightly channel

`.github/workflows/nightly.yml` runs daily, and on `workflow_dispatch`. It skips when `main` has not moved since the last nightly. Otherwise it builds `main` through `build-binaries.yml` with the version stamped as `X.Y.(Z+1)-nightly.YYYYMMDD`, where `X.Y.Z` is the version in `Cargo.toml`, publishes a GitHub prerelease tagged `nightly-YYYYMMDD`, pushes the sandbox images as `:nightly`, and deletes all but the newest `NIGHTLY_KEEP` nightlies with their tags. It does not need `release` environment approval and never publishes to ClawHub.

The tag stays outside `v*` so `release.yml`, `docker.yml`'s tag trigger and release-plz ignore it, and the version sorts above the stable release it builds on. The update check follows the running binary's channel: stable builds only see stable releases and nightly builds only see nightlies. A nightly release's version is read from its name, `Agent of Empires vX.Y.Z-nightly.YYYYMMDD`, because its tag only carries the date.

Once a migration is merged to `main`, its number and behavior are frozen: nightly users have already run it.

### Enabling nightlies

The workflow does nothing until the `NIGHTLY_ENABLED` repository variable is `true`. Stable clients up to v1.22.2 read the newest entry of the full release list, cannot parse a `nightly-*` tag as a version, and then report no update, so publishing a nightly hides every later stable release from them. Before turning nightlies on:

1. Ship a stable release with the channel-aware update check, and wait until most users have updated to it.
2. Exclude `refs/tags/nightly-*` from the `Tags` ruleset, whose deletion rule otherwise makes the prune step fail.
3. Give the homebrew-core `aoe` formula a `livecheck` regex such as `/^v?(\d+(?:\.\d+)+)$/i`, so `nightly-*` tags are not read as versions.

Then:

```bash
gh variable set NIGHTLY_ENABLED --body true -R agent-of-empires/agent-of-empires
```

The first nightly appears after the next scheduled run, or `gh workflow run nightly.yml -R agent-of-empires/agent-of-empires`. Once it is published, delete the "not published yet" note under Nightly Channel in `docs/installation.md`.

## Skill hubs

`aoe` ships a coding-agent management skill to two hubs from `contrib/`, and `cargo xtask check-skill` (run in CI) validates that both reference real CLI commands and follow each hub's frontmatter rules.

- **ClawHub** (`contrib/openclaw-skill/`) is published automatically by `release.yml` on every release. Its frontmatter must carry no top-level `version:`; ClawHub's `_meta.json` and the `--version` flag are the source of truth.
- **Hermes** (`contrib/hermes-skill/`) is PR-based and published by hand, so the in-repo copy is the source of truth and the hub copy drifts until you re-publish. Its frontmatter must carry a `version:`. Bump it, then run `hermes skills publish contrib/hermes-skill --to github --repo NousResearch/hermes-agent` (needs the `hermes` CLI and a GitHub token with fork permissions). Cadence is on change, not every release.

## Experimental terminal dependency

The Crossterm patch is an experiment in [draft PR #5593](https://github.com/agent-of-empires/agent-of-empires/pull/5593),
not an approved fork or supported-release commitment.
[Upstream report #1173](https://github.com/crossterm-rs/crossterm/issues/1173)
links a standalone reproduction and the candidate implementation.
Keep the PR draft until maintainers choose an official dependency that satisfies
the required behavior, redesign the affected change without the patch, or approve
a separate temporary exception. An exception must name a willing owner, exact
scope, expiry date or release, security obligations, and removal trigger.
Upstream silence does not create or renew a maintenance commitment.

`vendor/crossterm` comes from the published
[`crossterm-0.29.0.crate`](https://static.crates.io/crates/crossterm/crossterm-0.29.0.crate)
archive, SHA-256
`d8b9f2e4c67f833b660cdb0a3523065869fb35570177239812ed4c905aeff87b`.
Its recorded upstream commit is
`36d95b26a26e64b0f8c12edfe11f410a6d56a812` at repository root.
Download and verify that archive, extract it separately, then inspect the delta:

```sh
diff -ruN --exclude=Cargo.lock --exclude=target \
  "$temporary/crossterm-0.29.0" vendor/crossterm
```

The local delta is the Unix Mio consumer and its regression tests, standalone
workspace metadata, test-only dependencies, and removal of unnecessary closure
parentheses in `src/terminal/sys/unix.rs` (warning-only), filtered-event
conservation, and EventStream error/waker delivery. Parser behavior, optional
backends, and published feature choices remain intact. Saved controls retain
their order. Each TTY readiness permits one read; incomplete input retains its
parser bytes while polling the remaining deadline. EOF and hard read errors
reach the stream once, then end that stream. Its worker wakes the latest task.
Readiness is reevaluated through the existing registration and poll. This does
not eliminate every race with a concurrent competing input consumer.

Preserve the complete MIT notices while this source is present. When selecting
an official replacement, verify the relevant behavior rather than requiring the
same implementation. Remove the path patch, vendor tree, vendor-only CI/helper,
packaging references, and notices needed only by that source in the same cutover.
Abandoning the experiment or reaching an unrenewed exception deadline also
requires removal; do not make retirement depend solely on an upstream merge.

For experimental validation on Linux and native macOS:

```sh
python3 .github/scripts/test-vendored-crossterm.py
CARGO_BUILD_JOBS=1 cargo test --manifest-path vendor/crossterm/Cargo.toml \
  --locked --lib --features event-stream -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test --lib tui::terminal_output
CARGO_BUILD_JOBS=1 cargo test --lib child_handoff_restores_real_raw_input_before_blocked_screen_controls
```

The CI helper seeds the fork's ignored lockfile from the project lock, rejects
production dependency drift or a libc/use-dev-tty backend switch, and requires all
21 real-consumer regressions, plus the Linux-only PTY-master EIO case, to execute.
The standalone suite is not covered by root workspace tests. A forced ordering
of an actual fetched batch establishes a conditional consumer contract, not the
original macOS event order. The blocking stale cases verify completed
private-alias consumption, not end-to-end App handoff.
Root terminal checks and native kqueue coverage are still required.

Coverage is 16 real PTY controls plus two real Unix stream socketpair full-buffer
controls on both Linux and macOS. The latter retain the complete 1025-byte prequeue,
one actual 1024-byte read, and original one-byte tail. Their writer stays open
through the final zero-time poll. These two cases do **not** prove a 1024-byte
terminal read on Darwin; the other 16 cases retain their real terminal-mode checks.
Additional cases cover incomplete CSI timeout and cancellation with later
completion, socket EOF, Linux PTY-master EIO, skipped-event conservation, and
latest-task error delivery. Native macOS validation remains required.

The prototype release archives carry `LICENSE`, `THIRD_PARTY_NOTICES.md`, and all of `licenses/`.
Both prototype Nix packages include the patch's notice and install complete notices under
`$out/share/licenses/aoe`. Crane dependency builds restore real vendor sources and
original manifests after generating workspace stubs, using vendor-only copy
inputs so application edits do not invalidate the dependency cache. Dependency
metadata and Cargo vendoring come from the original manifest and lock, not dummy
sources. These packaging changes are not release approval. An approved temporary
exception would also require checking `nix build`, `nix build .#aoe-with-web`,
installed notices, and a generated release archive.

Before release, evaluate both configured packages and the development shell for
every system advertised by the Nix CI matrix with
`--option allow-import-from-derivation false`; evaluation must not realize foreign
generated sources.

To inspect the experiment, use a Git checkout and project lockfile
(`cargo install --path . --locked`). Ordinary registry packaging strips patches
and nested packages; it does not reproduce this draft. Neither route establishes
a supported distribution of the experimental patch.
