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

## Commit messages

One **imperative, single-sentence subject line — and nothing else**:

- No body, no bullets, no rationale. If it doesn't fit in one clear sentence, the
  commit is too big — split it.
- No trailers, no attribution — no `Co-Authored-By`, no "Generated with…", no
  sign-off.
- Capitalized, imperative mood, no trailing period.

Good: `Add case-insensitive BDMV directory lookup` ·
`Fix off-by-one in the MPLS chapter count`.

## Pull requests

Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md) — a short
checklist mirroring the rules above. Keep PRs focused; prefer several small
commits, each a coherent step that stands on its own.

By contributing you agree that your contributions are licensed under the
project's [LGPL-2.1-only](LICENSE) license.
