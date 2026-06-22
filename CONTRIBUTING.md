# Contributing to bdinfo-rs

Thanks for your interest in bdinfo-rs — a memory-safe, zero-C-dependency Rust
Blu-ray disc analyzer. This guide covers how to build, what CI expects, and the
handful of house rules that keep the project's guarantees intact.

## Reporting issues

- **Bugs and feature requests:** open an issue on the
  [tracker](https://github.com/agentjp/bdinfo-rs/issues). A disc structure or
  report excerpt that reproduces the problem helps a lot.
- **Security vulnerabilities:** report privately via
  [GitHub Security Advisories](https://github.com/agentjp/bdinfo-rs/security/advisories/new),
  not a public issue.

## Building

No C toolchain, no system libraries, no extra steps — the same on Windows,
macOS, and Linux:

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs
cargo build --workspace      # debug build; add --release for the optimized binary
cargo test --workspace       # unit + integration + property tests
```

The pinned Rust toolchain installs itself automatically via `rust-toolchain.toml`,
and Cargo fetches the few pure-Rust dependencies. The **minimum supported Rust
version (MSRV) is 1.96**.

## What CI checks

Every pull request must be green. CI mirrors the local quality gate and is
deliberately strict:

- **build + test** on Linux, Windows, and macOS.
- **rustfmt** (pinned nightly — the format config uses nightly-only options).
- **clippy** with `-D warnings` (the pedantic, nursery, cargo, and selected
  restriction lints are all on).
- **typos** spell-check, **machete** / **shear** unused-dependency checks, and a
  **doc** build with warnings treated as errors.
- **cargo-semver-checks** against the last release tag — the library API is
  SemVer-stable.
- **coverage** — 100% lines / regions / functions on the library.
- **cargo-deny** and **cargo-vet** — advisories, the license allow-list, the
  no-C-dependency bans, and per-dependency supply-chain audits.
- **conventional commits + banned words** — `convco check` verifies every commit
  on the PR is a [Conventional Commit](https://www.conventionalcommits.org/), and a
  banned-word gate rejects LLM/AI-attribution tokens in the commit messages and the
  PR title/body. See [Commit messages](#commit-messages--conventional-commits) below.

Running `cargo build`, `cargo test`, `cargo clippy`, and `cargo fmt --check`
locally before pushing catches the common failures early.

## House rules

These are not style preferences — each one protects a guarantee the project
makes, and a change that breaks one will not be merged.

- **No `unsafe`.** It is `forbid`-den workspace-wide; memory safety is the
  product.
- **No C / no FFI.** Never add a dependency that compiles or links C (a `cc` /
  `cmake` / `bindgen` build script, or a `-sys` crate wrapping a C library). The
  single static binary is the whole point, and `cargo-deny` enforces this.
- **Deterministic output.** Never use `HashMap` / `HashSet` on an output path —
  their iteration order is nondeterministic. Use `BTreeMap` or a sorted `Vec`.
- **Big-endian disc reads.** Disc structures are big-endian; always parse via
  `from_be_bytes`, never host byte order. (UDF, in `vfs::udf`, is little-endian
  by spec — the one exception.)
- **Never panic on input.** Malformed data returns `Result<_, BdError>`; an
  absent field or EOF is an `Option`. No `unwrap` / `expect` / `panic` / raw
  indexing on disc bytes — the parser must never panic or hang on hostile input.
- **The report is byte-locked.** The human-readable disc report is a frozen byte
  contract pinned by a fixture test. Do not change report bytes incidentally; a
  deliberate change (for example a spec-correctness fix) must arrive with the
  re-pinned fixture and a clear rationale.

## Commit messages — Conventional Commits

bdinfo-rs uses [**Conventional Commits**](https://www.conventionalcommits.org/).
master is **squash-merged**, so each PR lands as one commit whose subject is the **PR
title** — that subject feeds the generated changelog and the computed release version.
Write the PR title (and every commit) as release-note copy. The format is:

```text
<type>(<scope>): <description>
```

**type** (required) sets the changelog section and the SemVer impact. These are the
only accepted types — `convco check` rejects anything else. The **bump** column is
what `convco version --bump` derives automatically; convco only bumps for `feat`,
`fix`, and breaking changes, so the rest read as no automatic bump (a release that
wants one anyway overrides it — see [Cutting a release](#cutting-a-release-maintainers)):

| type | bump | changelog | use for |
|------|-------|-----------|---------|
| `feat` | minor | **Features** | a new user-visible capability |
| `fix` | patch | **Bug Fixes** | a bug fix |
| `perf` | — | **Performance** | a measurable speed / size improvement |
| `refactor` | — | hidden | a behaviour-preserving code change |
| `docs` | — | hidden | documentation only |
| `test` | — | hidden | tests only |
| `build` | — | hidden | build system / dependencies |
| `ci` | — | hidden | CI configuration / workflows |
| `chore` | — | hidden | housekeeping touching no src / tests |
| `style` | — | hidden | formatting only |

A `!` after the type/scope or a `BREAKING CHANGE:` footer forces a **major** bump.

**scope** (optional) names the area, from the closed vocabulary in
[`.convco`](.convco): code — `core cli vfs udf bitstream bytes stream bdrom mpls clpi
m2ts index discovery codec report`; infrastructure — `deps ci release docker fuzz
packaging gate`. Adding a module means adding its scope to `.convco`.

**description** (required) — imperative, lower-case start, no trailing period; a full
release-note sentence.

**breaking changes** — append `!` after the type / scope (`feat(report)!: …`) or add
a `BREAKING CHANGE:` footer; either bumps the **major** version.

```text
feat(udf): add case-insensitive descriptor lookup
fix(mpls): correct the off-by-one in the chapter count
perf(m2ts): halve the allocations in the packet scan
feat(report)!: drop the legacy column from the locked report
```

**No attribution, ever** — no `Co-Authored-By`, no "Generated with…", no sign-off,
and no LLM/AI-attribution words (`Claude`, `Copilot`, `GPT`, `AI`, the 🤖 emoji, …)
anywhere in a commit message or in the PR title / body. This is enforced mechanically
by [`.github/scripts/check-banned-words.ps1`](.github/scripts/check-banned-words.ps1),
which runs both in the commit-msg hook and in CI.

### Authoring and checking locally

Install [convco](https://convco.github.io/) as a prebuilt binary (`cargo binstall
convco`, or `taiki-e/install-action`; avoid the from-source build, which compiles C —
convco is a dev tool, never a bdinfo-rs dependency). Write the subject by hand, or let
convco scaffold it:

```sh
convco commit --feat --scope udf -m "add case-insensitive descriptor lookup"
```

Validate before pushing — the same two checks CI runs:

```sh
convco check origin/master..HEAD          # every commit is conventional
git log --format=%B origin/master..HEAD | pwsh .github/scripts/check-banned-words.ps1 -Label commits
```

Maintainers can install git hooks (commit-msg + pre-push) that run both automatically
via the local `scripts/install-hooks.ps1`.

## Pull requests

Open a PR from a branch — never push to master, which is protected. Keep the branch
**rebased** on `origin/master` (master keeps a **linear history**).

- master is **squash-merged**: the whole PR lands as ONE commit whose subject is the **PR
  title**, so the **PR title must be a Conventional Commit** — it is the changelog line and
  the version driver, and CI enforces it (a required check). Every commit in the PR must
  still be conventional (hygiene, and a single-commit PR's message is the squash fallback),
  but the individual commits do not appear on master.
- **Why squash, not rebase:** master requires **signed commits** AND **linear history**.
  GitHub cannot sign rebased commits, and merge commits are non-linear — so squash, which
  GitHub signs (Verified) and keeps linear, is the only method compatible with both.
- All required checks must be green, including **conventional commits + banned words**.

Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md).

By contributing you agree that your contributions are licensed under the project's
[LGPL-2.1-only](LICENSE) license.

## Cutting a release (maintainers)

convco computes the version and changelog, but it never commits, tags, or pushes — its
output is advisory and the pushed `vX.Y.Z` tag is the source of truth. Merging a PR never
triggers a release; only the tag does (it runs cargo-dist). Before tagging:

1. **Compute the next version (advisory):**

   ```sh
   convco version --bump          # e.g. 1.1.0; override with --patch / --minor / --major
   ```

2. **Set it across the workspace** and bump the internal pin, then refresh the lockfile:

   ```sh
   cargo set-version --workspace <X.Y.Z>
   # then bump [workspace.dependencies] bdinfo-rs-core = { …, version = "X.Y.Z" }
   cargo build
   ```

3. **Generate the new CHANGELOG section** from the commits since the last tag and insert
   it above the existing history — the curated pre-adoption entries are preserved, only
   the new version's section is generated:

   ```sh
   convco changelog v<previous>..HEAD
   ```

   convco emits the bracketed Keep-a-Changelog format (`## [X.Y.Z](…) (date)` with
   `### Features` / `### Bug Fixes` sections) that cargo-dist parses for the release
   notes — the same heading shape as the existing `## [1.0.0]` entry. convco 0.6.4 derives
   the link host from the git remote (the `ai.github.com` SSH alias), so rewrite
   `ai.github.com` → `github.com` in the generated links; the cockpit
   `scripts/release-prep.ps1` does steps 1–3, the rewrite included, in one pass.

4. Run the gate, open a PR (its title is the conventional release commit), **Squash and
   merge**, then push the `vX.Y.Z` tag on master.
