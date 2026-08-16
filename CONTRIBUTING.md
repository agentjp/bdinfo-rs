# Contributing to bdinfo-rs

This guide covers how to build, how to run the same checks CI runs, and the house rules that keep
the project's guarantees intact.

## Reporting issues

- **Output differences** — the most useful report there is. Use the
  [output difference template](https://github.com/agentjp/bdinfo-rs/issues/new?template=output_difference.yml)
  and paste the **text report** (codecs, bitrates, stream layout). Never attach audio or video.
- **Bugs and feature requests** — the [tracker](https://github.com/agentjp/bdinfo-rs/issues).
- **Security vulnerabilities** — privately, via
  [GitHub Security Advisories](https://github.com/agentjp/bdinfo-rs/security/advisories/new).
  See [SECURITY.md](SECURITY.md); a panic or hang on malformed input is in scope.

## Building

No C toolchain, no system libraries, no extra steps — the same on Windows, macOS, and Linux. The
pinned toolchain installs itself via `rust-toolchain.toml`; **MSRV is 1.96**.

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs
cargo build --workspace      # add --release for the optimized binary
cargo test --workspace
```

The desktop app and the WebAssembly package are **separate workspaces** — `--workspace` at the root
does not touch them, and each has its own lockfile and its own gate:

```sh
cd crates/bdinfo-rs-gui  && cargo build --release
cd crates/bdinfo-rs-wasm && cargo test
```

## Running the checks locally

The gate is a sequence of tracked Cargo aliases in [`.cargo/config.toml`](.cargo/config.toml) — CI
runs these exact aliases, so local and CI agree. Run them cheapest first and stop at the first
failure:

| | | |
|---|---|---|
| `cargo ck` | type-check every target | `cargo dc` | docs, warnings as errors |
| `cargo nt` | tests (nextest, strict) | `cargo mc` | unused dependencies |
| `cargo lt` | clippy, `-D warnings` | `cargo dn` | cargo-deny: advisories, licenses, bans |
| `cargo cov` | coverage, floors at 100% | `cargo mt` | mutation testing (slow) |

Two checks have no alias:

- **Formatting** needs the pinned **nightly** rustfmt (`rustfmt.toml` uses nightly-only options),
  and an alias cannot select a toolchain: `cargo +nightly fmt --all --check`.
- **Mutation testing on a diff** is what a pull request runs:
  `cargo mutants --in-place --in-diff <diff>`.

## House rules

Each of these protects a guarantee the project makes. A change that breaks one will not be merged.

- **No `unsafe`.** `forbid`-den workspace-wide, including the desktop app. Memory safety is the
  product. (The WebAssembly crate carries a single tripwire-guarded exception for its bindings.)
- **No C, no FFI.** Never add a dependency that compiles or links C — a `cc` / `cmake` / `bindgen`
  build script, or a `-sys` crate wrapping a C library. `cargo-deny` enforces it.
- **Checked arithmetic.** Bare `+ - * << >>` is rejected. Use `wrapping_*` for fixed-width codec and
  bit math; `checked_*` with `?` for buffer offsets and lengths, where overflow means malformed
  input.
- **Big-endian disc reads.** Disc structures are big-endian — always `from_be_bytes`. UDF, in
  `vfs::udf`, is little-endian by specification: the one exception.
- **Deterministic output.** Never `HashMap` / `HashSet` on an output path. Use `BTreeMap` or a
  sorted `Vec`.
- **Never panic on input.** Malformed data returns `Result<_, BdError>`; an absent field or EOF is
  an `Option`. No `unwrap` / `expect` / `panic` / raw indexing on disc bytes.
- **The report is byte-locked.** The disc report is a frozen byte contract (CRLF, UTF-8 without BOM,
  invariant numbers, truncated times) pinned by fixture tests. A deliberate change must arrive with
  the re-pinned fixture and a rationale — and if it is a divergence from the original BDInfo, an
  entry in [DIFFERENCES.md](DIFFERENCES.md).
- **Style belongs to the tools.** rustfmt owns layout, clippy owns idioms. Never hand-tune spacing
  or import order.

A module is considered done at 100% coverage with zero surviving mutants, carrying both unit tests
and property tests.

## The four-surface option contract

Every user-facing option exists on four surfaces: the `bdinfo-rs-core` library and the three
front-ends over it. This table is the contract — presence, default, and any deliberate cut.

| Option | `bdinfo-rs-core` | Command line | Desktop app | Browser package |
|---|---|---|---|---|
| Short-playlist threshold | `PlaylistFilter::short_playlist_seconds`, default `20.0`; the domain's ceiling is `MAX_SHORT_PLAYLIST_SECONDS` (86400) | `--short-playlist-seconds`, default 20; outside 0 to 86400 fails at argument parsing (exit 2) | `short-playlist-seconds`, default 20; out-of-range values are clamped, not rejected | `shortPlaylistSeconds` on `inspect` and `scan`, default 20; negative, non-finite or past the ceiling throws |
| Hide short playlists | `PlaylistFilter::filter_short_playlists`, default `true` | on by default, `--show-short-playlists` turns it off | `filter-short-playlists`, default `true` | **Cut** — a playlist is never withheld. Each carries `hiddenBy`; the consumer filters client-side |
| Hide looping playlists | `PlaylistFilter::filter_looping_playlists`, default `true` | on by default, `--show-looping-playlists` turns it off | `filter-looping-playlists`, default `true` | **Cut** — as above |
| Keep partially scanned stream files | `ScanOptions::keep_partial`, default `true` | kept by default, `--drop-partial` discards | `keep-partial-scans`, default `true` | `keepPartial` on `scan`, default `true` |
| `STREAM DIAGNOSTICS` section | `RenderOptions::stream_diagnostics`, default `true` | rendered by default, `--no-stream-diagnostics` omits it | `report-stream-diagnostics`, default `true` | `streamDiagnostics` on `scan` and `renderReport`, default `true` |
| `QUICK SUMMARY` section | `RenderOptions::quick_summary`, default `true` | rendered by default, `--no-quick-summary` omits it | `report-quick-summary`, default `true` | `quickSummary` on the same two calls, default `true` |
| Scan depth | `ScanMode::Metadata` / `Codecs` / `Full` | `Metadata` for `--list`, `Full` for a measured run; `Codecs` unused | `Metadata` on open, then `Codecs` unless the disc is encrypted; `Full` for the scan | `inspect` is `Metadata`, or `Codecs` with `codecs: true`; `scan` is `Full` |
| Cancellation | the `AtomicBool` the packet scan polls per read chunk | <kbd>Ctrl</kbd>+<kbd>C</kbd> on the styled progress path, exit 130 | the Cancel button during a scan | **Cut** — no cooperative cancel; `signal` terminates the Worker, the idiomatic browser cancel |
| AACS-encrypted disc | `BdRom::is_aacs_encrypted`; the library scans one when asked | refused before the picker, exit 4; `--list` still works | scan action disabled, the disc marked encrypted | `Disc.isAacsEncrypted` and no policy — the consumer decides |
| Playlist selection | an explicit index order into `bdrom.playlists` | `--mpls` (unfiltered, in the given order), `--whole`, or the interactive picker | table checkboxes; scanning with none checked scans the whole disc | `selection` on `scan` (names, unfiltered); omitted or empty measures the standard set |

The desktop app's settings are keys in `gui.conf`; the browser package's are fields of the options
object every call takes.

**Any change to an option in this table updates the table in the same pull request** — a new
option, a changed default, a widened domain, a cut. Its absence is how the surfaces drifted apart
before it existed.

## Commit messages

bdinfo-rs uses [Conventional Commits](https://www.conventionalcommits.org/). master is
**squash-merged**, so each pull request lands as one commit whose subject is the **pull request
title** — that subject feeds the generated changelog and the computed version. Write it as
release-note copy.

```text
<type>(<scope>): <description>
```

**type** sets the changelog section and the SemVer impact. This list is closed; `convco check`
rejects anything else. Only `feat`, `fix`, and breaking changes bump the version automatically.

| type | bump | changelog | use for |
|---|---|---|---|
| `feat` | minor | **Features** | a new user-visible capability |
| `fix` | patch | **Bug Fixes** | a bug fix |
| `perf` | — | **Performance** | a measurable speed or size improvement |
| `refactor` | — | hidden | a behaviour-preserving code change |
| `docs` | — | hidden | documentation only |
| `test` | — | hidden | tests only |
| `build` | — | hidden | build system or dependencies |
| `ci` | — | hidden | CI configuration or workflows |
| `chore` | — | hidden | housekeeping touching no src or tests |
| `style` | — | hidden | formatting only |

A `!` after the type or scope, or a `BREAKING CHANGE:` footer, forces a **major** bump.

**scope** is optional, from the vocabulary in [`.convco`](.convco) — code: `core cli vfs udf
bitstream bytes stream bdrom mpls clpi m2ts index discovery codec report`; front-ends: `wasm gui`;
infrastructure: `deps ci release docker fuzz packaging gate`. Adding a module means adding its
scope.

**description** is imperative, lower-case, with no trailing period — a full release-note sentence.

```text
feat(udf): add case-insensitive descriptor lookup
fix(mpls): correct the off-by-one in the chapter count
perf(m2ts): halve the allocations in the packet scan
feat(report)!: drop the legacy column from the locked report
```

**No attribution, ever.** No `Co-Authored-By`, no "Generated with…", no sign-off, and no
LLM/AI-attribution tokens anywhere in a commit message or a pull request title or body. Enforced by
[`.github/scripts/check-banned-words.ps1`](.github/scripts/check-banned-words.ps1), which runs in
both the commit-msg hook and CI.

Check both before pushing — the same two things CI runs:

```sh
convco check origin/master..HEAD
git log --format=%B origin/master..HEAD | pwsh .github/scripts/check-banned-words.ps1 -Label commits
```

Install [convco](https://convco.github.io/) as a prebuilt binary (`cargo binstall convco`) — the
from-source build compiles C, and it is a dev tool, never a bdinfo-rs dependency.

## Pull requests

Open a pull request from a branch; master is protected. Keep the branch **rebased** on
`origin/master`, which branch protection requires to be up to date before merging.

- The **pull request title must be a Conventional Commit** — it becomes the squashed commit subject,
  the changelog line, and the version driver. It is a required check.
- Every commit in the branch must also be conventional. They do not appear on master, but a
  single-commit pull request's message is the squash fallback.
- **Why squash rather than rebase:** master requires both signed commits and linear history. GitHub
  cannot sign rebased commits, and merge commits are not linear. Squash is the only method
  satisfying both, and GitHub signs it.
- All required checks must be green.

Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md).

By contributing you agree that your contributions are licensed under the project's
[LGPL-2.1-or-later](LICENSE).

## What CI checks

Every pull request runs a plan-gated orchestrator: a classifier job maps the diff to areas, and only
the affected gate jobs run. The full set:

- **Build and test** on Linux, Windows, and macOS.
- **rustfmt** (pinned nightly) and **clippy** with `-D warnings` — pedantic, nursery, cargo, and a
  selected restriction set.
- **Coverage** — 100% lines, regions, and functions on the library, plus branch-coverage floors.
- **Mutation testing** on the diff, and a full sweep daily and on every release tag.
- **Fuzzing** — the committed seed corpus is replayed on every pull request; fresh fuzzing runs
  nightly over a corpus that persists between runs, and again on every release tag. Both run at
  64- and 32-bit `usize`, the second being the width the browser package ships.
- **typos**, **machete** and **shear** (unused dependencies), and a **doc** build with warnings as
  errors.
- **cargo-semver-checks** against the last release tag — the library API is SemVer-stable.
- **cargo-deny** and **cargo-vet** — license allow-list, the no-C-dependency ban, crates.io-only
  sources, and per-dependency audits. RustSec advisories run in a daily audit, and a pull request's
  own dependency changes go through dependency-review, so a freshly published CVE never reddens an
  unrelated pull request.
- **Conventional commits and banned words**, and CodeQL.

## Cutting a release (maintainers)

Releases are tag-driven. convco computes the version and changelog but never commits, tags, or
pushes — its output is advisory and the pushed `vX.Y.Z` tag is the source of truth. Merging never
releases; only the tag does.

1. **Compute the next version** — `convco version --bump`, overriding with `--patch` / `--minor` /
   `--major` where convco's rules do not apply (it bumps only on `feat`, `fix`, and breaking
   changes).
2. **Set it across the workspace** and bump the internal pin, then refresh the lockfile:

   ```sh
   cargo set-version --workspace <X.Y.Z>
   # then bump [workspace.dependencies] bdinfo-rs-core = { …, version = "X.Y.Z" }
   cargo build
   ```

3. **Set it in every other version-bearing file.** The two excluded sibling workspaces and the npm
   package are outside `cargo set-version`'s reach, and four documents carry the number as prose.
   The complete list:

   | File | What carries the version |
   |---|---|
   | `Cargo.toml` | the workspace version and the `[workspace.dependencies] bdinfo-rs-core` pin |
   | `crates/bdinfo-rs-wasm/Cargo.toml` | the crate version |
   | `crates/bdinfo-rs-wasm/web/package.json` | `version`, mirrored into `package-lock.json` |
   | `crates/bdinfo-rs-gui/Cargo.toml` | the crate version and its own `bdinfo-rs-core` pin |
   | `SECURITY.md` | the supported-versions table |
   | `INSTALL.md` | the Docker pin example and the rolling-tag sentence |
   | `.github/ISSUE_TEMPLATE/bug_report.yml`, `gui_bug_report.yml`, `output_difference.yml` | the version-field placeholders |
   | `crates/bdinfo-rs-wasm/Cargo.lock`, `crates/bdinfo-rs-gui/Cargo.lock`, `fuzz/Cargo.lock` | each sibling lockfile records the workspace crate versions; a `cargo check` in that workspace refreshes it |

   The npm tag guard cross-checks the workspace version, the wasm crate version and
   `package.json` against the pushed tag, so a missed one fails the release rather than shipping.

4. **Generate the changelog section** — `convco changelog v<previous>..HEAD` — and insert it above
   the preserved history. convco emits the bracketed Keep-a-Changelog shape that cargo-dist parses
   for the release notes. convco derives link hosts from the git remote, so check the generated
   links point at `github.com`.
5. Run the gate, open a pull request whose title is the release commit, squash-merge, then push the
   `vX.Y.Z` tag on master. The tag must equal the workspace version and sit on master.

Three files are **release-prep, done last**, because they carry the release date rather than only
the number: the `CHANGELOG.md` section (step 4 above; cargo-dist derives the GitHub Release notes
from it, so it must exist before the tag), the AppStream metainfo release entry
(`crates/bdinfo-rs-gui/packaging/io.github.agentjp.bdinfo-rs.metainfo.xml` — copied verbatim into
the AppImage, `.deb` and `.rpm`, and `appstream-util validate-relax` does not catch a stale entry),
and `CITATION.cff` (version and date).

The tag fans out to every channel automatically. Releases are **immutable** — never re-push a tag;
recover a partial release per channel.

### The GUI release lane (`gui-v*`)

The desktop app releases through its own hand-rolled lane (`gui-release.yml`), not cargo-dist: a
`gui-vX.Y.Z` tag builds the six native targets, packages them (Windows `.msi` + portable `.zip`,
macOS `.dmg`, Linux AppImage + `.deb` + `.rpm`), and publishes a GitHub Release marked not-latest.
cargo-dist's `v` tag-namespace keeps the two lanes from ever firing each other.

The publishing that follows is split by how each channel authenticates, mirroring the command-line
lane. `gui-publish-crates.yml` puts the crate on crates.io from the tag push, because crates.io
Trusted Publishing refuses to mint a token for a `workflow_run` trigger; `gui-publish.yml` then
pushes the release's attested bytes to WinGet, the Homebrew cask, the AUR and Cloudsmith on
`workflow_run`, using stored secrets. Both crates.io lanes bind the `crates-io` GitHub environment,
which holds no secrets and exists so the minted token's OIDC claim names it — the trusted publisher
records the workflow filename *and* that environment, so pushing a workflow is not by itself enough
to publish a crate. `.github/scripts/check-trusted-publishing-triggers.ps1` fails CI if any
`workflow_run` workflow tries to mint a registry token.

**Versioning: two lanes, one version.** The library, the command line, the browser package and the
desktop app move in lock-step: every surface carries the same `X.Y.Z`, and a release tags both
lanes at it (`vX.Y.Z` and `gui-vX.Y.Z`). The lanes are separate because they build and publish
different artifacts, not because the versions may drift apart. The invariant the GUI lane enforces:
`gui-vX.Y.Z` must equal `crates/bdinfo-rs-gui/Cargo.toml`'s version and sit on master.

Before tagging, run the lane's dry run: Actions → gui-release → Run workflow (on master). It builds
and packages all six targets and uploads the results as workflow artifacts for hand-testing; only
release creation and attestation are tag-only. A tag run always rebuilds from the tagged commit —
it never reuses dry-run artifacts.
