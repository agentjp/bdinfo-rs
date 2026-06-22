# bdinfo-rs ships as one fully-static, zero-C-dependency musl binary, so the
# runtime image is literally that binary on `scratch`: no libc, no shell, no OS —
# a ~2 MB image with nothing to patch and no attack surface beyond the binary.
#
# The image ships the EXACT binary the release pipeline already cut, not a separate
# rebuild: docker.yml downloads the published `*-unknown-linux-musl.tar.gz` archives
# from the GitHub Release and stages each arch's binary under `bin/<arch>/` before
# this build. So the binary inside the image is byte-for-byte the one users
# download — same SHA-256, same cargo-auditable `.dep-v0` section, covered by the
# SAME Sigstore attestation — with nothing to "prove equivalent" because it IS the
# same artifact. cargo-dist owns that binary (release.yml); here we only package it.
#
# There is no build stage and no RUN: the image only COPYs a prebuilt binary, so a
# single `docker buildx --platform linux/amd64,linux/arm64` build needs no QEMU and
# emits the multi-arch manifest list directly. TARGETARCH is the buildx-provided
# per-platform arch (`amd64` | `arm64`), so each platform copies its own binary.
#
# Build a local image from a checkout by staging a binary first, then building that
# one arch, e.g.:
#   cargo build --release -p bdinfo-rs
#   mkdir -p bin/amd64 && cp target/release/bdinfo-rs bin/amd64/bdinfo-rs
#   docker build --platform linux/amd64 .
FROM scratch
ARG TARGETARCH
COPY bin/${TARGETARCH}/bdinfo-rs /bdinfo-rs
ENTRYPOINT ["/bdinfo-rs"]
