#!/usr/bin/env bash
# Build the Linux distro packages (.deb + .rpm, for x86_64 AND aarch64) from the
# static musl binaries dist already built, and stage them under dist-extra/ for
# dist's `extra-artifacts` (dist-workspace.toml) to attach to the GitHub Release.
#
# WHY THIS LIVES INSIDE THE RELEASE: the packages are now release assets attached
# AT release creation, before the release is published — so the release is
# compatible with GitHub "immutable releases" (which freeze a release's assets at
# publication; an after-the-fact `gh release upload` would be rejected). The old
# packages.yml built+uploaded the .deb/.rpm AFTER the release, which immutability
# forbids; that build moved here. packages.yml now only DOWNLOADS these assets to
# install-verify them on a native runner and push them to Cloudsmith.
#
# WHERE THIS RUNS: dist's build-global-artifacts job (ubuntu, x86_64). At that
# point dist has downloaded the per-target release archives to target/distrib/ but
# the raw compiled binaries are not on disk — so we extract each binary (plus the
# man page + shell completions) from its archive, stage it exactly where
# [package.metadata.deb]/[package.metadata.generate-rpm] expect, and repackage.
# cargo-deb / cargo-generate-rpm are pure Rust (no dpkg / rpmbuild needed) and here
# only REPACKAGE a prebuilt binary (--no-build), so cross-arch packaging on an
# x86_64 host is fine — the package's internal arch label comes from --target, not
# the host. The output filenames are stable (triple-based, matching dist's archive
# names) so dist's `extra-artifacts.artifacts` can list literal paths and
# packages.yml can download them by a fixed pattern.
set -euo pipefail

TRIPLES=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
OUT=dist-extra
mkdir -p "$OUT"

# The pure-Rust packagers, compiled from source. Unpinned (latest), matching the
# old packages.yml posture; there is no prebuilt-binary URL/SHA to rot, so this is
# zero-maintenance. It runs only on a release tag, so the one-time compile is fine.
# Skip if a cached copy is already on PATH (re-runs / future dist caching).
command -v cargo-deb >/dev/null 2>&1 && command -v cargo-generate-rpm >/dev/null 2>&1 \
  || cargo install --locked cargo-deb cargo-generate-rpm

for triple in "${TRIPLES[@]}"; do
  archive="target/distrib/bdinfo-rs-${triple}.tar.gz"
  test -f "$archive" || { echo "::error::missing release archive $archive"; exit 1; }
  tar xzf "$archive" -C target/distrib
  # dist wraps unix archives in a <pkg>-<triple>/ directory.
  d="target/distrib/bdinfo-rs-${triple}"

  # Stage exactly where the package metadata's `assets` expect: the binary at
  # target/<triple>/release/ (cargo-deb / cargo-generate-rpm rewrite the
  # target/release/ asset prefix under --target), and the man page + completions
  # under release-assets/ — which cargo-deb resolves relative to the crate dir,
  # hence the second copy into crates/bdinfo-rs/release-assets/.
  mkdir -p "target/${triple}/release" release-assets crates/bdinfo-rs/release-assets
  cp "$d/bdinfo-rs" "target/${triple}/release/bdinfo-rs"
  for f in bdinfo-rs.1 bdinfo-rs.bash _bdinfo-rs bdinfo-rs.fish; do
    cp "$d/$f" release-assets/
    cp "$d/$f" crates/bdinfo-rs/release-assets/
  done

  cargo deb --no-build --no-strip --target "$triple" -p bdinfo-rs
  cp target/"${triple}"/debian/*.deb "$OUT/bdinfo-rs-${triple}.deb"

  cargo generate-rpm -p crates/bdinfo-rs --target "$triple"
  cp target/"${triple}"/generate-rpm/*.rpm "$OUT/bdinfo-rs-${triple}.rpm"
done

echo "Staged Linux packages for the release:"
ls -l "$OUT"
