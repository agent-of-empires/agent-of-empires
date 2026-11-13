# Releases

Maintainer reference for how `agent-of-empires` ships: stable releases cut from a release PR, plus a daily nightly channel.

## Stable releases

`.github/workflows/release-plz.yml` runs [release-plz](https://release-plz.dev) daily and on `workflow_dispatch`. It keeps one release PR open, branch `release-plz-*` and label `release`, that bumps `Cargo.toml` and `Cargo.lock` and prepends the new section to `CHANGELOG.md`. To ship, dispatch the workflow so the PR covers everything on `main`, then merge it. When it merges, release-plz pushes the `vX.Y.Z` tag on the PR's own squash commit with `RELEASE_TOKEN`, and the tag push starts `release.yml`. That workflow waits for `release` environment approval, builds the platform binaries through `build-binaries.yml`, publishes the GitHub release with the version's `CHANGELOG.md` section as its body, and publishes the ClawHub skill. `docker.yml` pushes the sandbox images for the same tag. If the `Tag release` job warns, the commits it lists merged after the PR's last refresh: they ship in the tag but are missing from `CHANGELOG.md`, so add them to that version's section in a follow-up PR and to the GitHub release notes.

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

## Vendored terminal dependency

Repository maintainers own the Crossterm patch, security updates, and upstreaming
decision; the release approver owns its checks. Review upstream releases and
advisories at each weekly release review and immediately for a relevant advisory.
Dependency-update PRs do not update path-patched source.

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

Only the Unix Mio consumer and its regression tests change, plus standalone
workspace metadata and test-only dependencies. Existing upstream tests, parser,
optional backends, and published feature choices remain intact. Saved control
events retain their order; a TTY hint retained across a return cannot authorize a
blocking read.
The patch reevaluates readiness through the existing registration and poll.
It does not eliminate every race with a concurrent competing input consumer.

For an update, extract the new archive independently, record its checksum here,
replay only the reviewed delta, and compare the pristine and patched trees.
Review upstream release notes, Rust requirements and advisories. Keep the full
MIT copyright notice in both the fork and `licenses/`, and update
`THIRD_PARTY_NOTICES.md`. Propose the reusable consumer correction upstream;
retire the patch only when the upstream version passes the same live, stale,
control-order and zero-time regressions without changing backends.

Run these checks sequentially before release on Linux and native macOS:

```sh
python3 .github/scripts/test-vendored-crossterm.py
CARGO_BUILD_JOBS=1 cargo test --manifest-path vendor/crossterm/Cargo.toml \
  --locked --lib --features event-stream -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test --lib tui::terminal_output
CARGO_BUILD_JOBS=1 cargo test --lib child_handoff_restores_real_raw_input_before_blocked_screen_controls
```

The CI helper seeds the fork's ignored lockfile from the project lock, rejects
production dependency drift or a libc/use-dev-tty backend switch, and requires all
18 real-consumer regressions to execute. The standalone suite is not covered by
root workspace tests. A forced ordering of an actual fetched batch establishes a
conditional consumer contract, not the original macOS event order. The blocking
stale cases verify completed private-alias consumption, not end-to-end App handoff.
Root terminal checks and native kqueue coverage are still required.

Release archives carry `LICENSE`, `THIRD_PARTY_NOTICES.md`, and all of `licenses/`.
Both Nix packages include the fork's notice and install complete notices under
`$out/share/licenses/aoe`. Crane dependency builds restore real vendor sources and
original manifests after generating workspace stubs, using vendor-only copy
inputs so application edits do not invalidate the dependency cache. Dependency
metadata and Cargo vendoring come from the original manifest and lock, not dummy
sources. Check `nix build` and `nix build .#aoe-with-web`, installed notices, and a
generated release archive before approval.

Before release, evaluate both configured packages and the development shell for
every system advertised by the Nix CI matrix with
`--option allow-import-from-derivation false`; evaluation must not realize foreign
generated sources.

Supported source installation uses the Git checkout and project lockfile
(`cargo install --path . --locked`, or the Git-based installer). Ordinary registry
packaging strips patches and nested packages and is not a supported source
distribution for this fork.
