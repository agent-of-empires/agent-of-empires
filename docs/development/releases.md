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

## Vendored parser forks

Repository maintainers own both forks, including security fixes and Unicode data;
the release approver owns the checks below. Review upstream releases and security
advisories at each weekly release review, and immediately for a relevant advisory.
Dependency-update PRs do not refresh path-patched sources.

The exact bases are the published crates.io archives, not a moving Git branch:

| Fork | Base archive | SHA-256 |
|---|---|---|
| pulldown-cmark | `https://static.crates.io/crates/pulldown-cmark/pulldown-cmark-0.13.6.crate` | `3db2be46835f013dd3f4fa7f5f7402c537787b54ebb6fe2b94bbdd7e2669bb7b` |
| unicode-width | `https://static.crates.io/crates/unicode-width/unicode-width-0.2.2.crate` | `b4ac048d71ede7ee76d585517add45da530660ef4390e49b098733c6e897f254` |

Their archive VCS identities are pulldown-cmark
`014b61d38d235f382935bb676bab872b7680afa2` (repository subdirectory
`pulldown-cmark`) and unicode-width
`9d98411769fe13c7c18cab0b3fbbab29ba8350ea` (repository root).

To reproduce the local delta, download each archive into a temporary directory,
verify the checksum above with `sha256sum`, extract with `tar -xzf`, then run:

```sh
diff -ruN "$temporary/pulldown-cmark-0.13.6" vendor/pulldown-cmark
diff -ruN "$temporary/unicode-width-0.2.2" vendor/unicode-width
```

Archive packaging metadata, omitted upstream-only files, and standalone workspace
manifests are not parser changes. Review those separately from the source delta.
Pulldown's local delta adds shared memory/disk input, disk-backed parser storage,
reference lookup and text normalization. Unicode-width's delta exposes incremental
reverse-width accumulation; generated width data and upstream expectations must
not change as a side effect. Keep upstream grammar tests intact. Do not regenerate
Unicode tables merely to refresh packaging. Record any intentional grammar,
Unicode-version, generated-table, or upstream-test change explicitly in the update PR.
The Unicode-width upstream suite and its `emoji-test.txt` fixture are retained
unchanged under `vendor/unicode-width/tests/`; the parser's upstream suites remain
under `vendor/pulldown-cmark/tests/`. Run them from their standalone manifests,
not only as dependencies of the root package.

For an update, extract the new pristine archive separately, record its URL and
checksum here, replay only the reviewed local source changes, then compare both
old/new upstream trees and the final fork. Review upstream release notes, Rust
minimum version, advisories, and Unicode release notes. Copy updated license texts
to `licenses/` and update `THIRD_PARTY_NOTICES.md`. Maintainers must decide in that
PR whether to propose the reusable disk-input and incremental-width APIs upstream.
Until upstream can satisfy bounded-storage and parity requirements, retain the
forks; do not replace them with stock crates merely to clear an update warning.

Run these release checks sequentially, on Linux and native macOS for terminal checks:

```sh
CARGO_BUILD_JOBS=1 cargo test --manifest-path vendor/pulldown-cmark/Cargo.toml --features disk,html
CARGO_BUILD_JOBS=1 cargo test --manifest-path vendor/unicode-width/Cargo.toml
CARGO_BUILD_JOBS=1 cargo test --manifest-path vendor/unicode-width/Cargo.toml --no-default-features
CARGO_BUILD_JOBS=1 cargo test --lib tui::rich_text
CARGO_BUILD_JOBS=1 cargo test --lib tui::structured_view
CARGO_BUILD_JOBS=1 cargo test --lib tui::terminal_output
CARGO_BUILD_JOBS=1 cargo test --lib child_handoff_restores_real_raw_input_before_blocked_screen_controls
```

The standalone vendor suites are not covered by root workspace tests. Pulldown's
upstream CommonMark/HTML suite needs `html`, unlike the production `disk` build.
Require memory/disk event and byte parity, split-grapheme width parity, cancelled
rendering, bounded retained storage, and exact frame/control ordering to remain
green. A root-only test result is not evidence for the upstream suites.

Release archives must contain `LICENSE`, `THIRD_PARTY_NOTICES.md`, and the complete
`licenses/` directory, including both fork MIT notices and Unicode-width copyright.
Nix source filtering explicitly includes upstream extensionless notices; both Nix
packages install notices under `$out/share/licenses/aoe`. Check both `nix build`
and `nix build .#aoe-with-web`, then inspect their installed license directories
and one generated release archive before approval.
Crane dependency-only builds must restore real vendored patch sources and original
manifests after generating workspace stubs: registry dependencies call their APIs.
Keep that restore hook's input vendor-only so application edits retain the dependency cache.
Dependency derivations also take metadata and Cargo vendoring from the original
manifest and lockfile, never from generated dummy sources. Before release, evaluate
both configured packages and the development shell for every system advertised by
the [Nix CI matrix](../../.github/workflows/ci.yml) with
`--option allow-import-from-derivation false`; cross-system evaluation must not
try to realize foreign generated sources.

Supported source installation uses this repository's Git checkout and lockfile
(`cargo install --path . --locked`, or the project's Git-based installer).
Ordinary registry packaging strips patch declarations and nested packages; a
registry archive is not a supported source distribution for these path forks.
Do not publish one without first defining and testing a distribution that carries
the forks and preserves their dependency resolution.
