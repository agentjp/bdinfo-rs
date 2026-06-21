# bdinfo-rs is one fully-static, zero-C-dependency musl binary, so the runtime
# image is literally just that binary on `scratch`: no libc, no shell, no OS —
# a ~2 MB image with nothing to patch and no attack surface beyond the binary.
#
# The build stage runs on the *target* platform and builds that arch's musl
# binary natively (the matching system linker handles the static link; Rust adds
# no C of its own). So `docker build` works as-is on amd64 and arm64 hosts, and
# CI gets a multi-arch image by building each arch on its own native runner.

# --- build: compile the static musl binary --------------------------------
# `cargo auditable build` (not plain `cargo build`) embeds the dependency tree in
# the binary's `.dep-v0` section, so the GHCR image is auditable by the same tools
# as the release binaries (`cargo audit bin`, trivy, grype) — matching the
# cargo-auditable=true that release.yml's dist build uses. Pure Rust, no C, no
# runtime dep: the FROM-scratch static guarantee is unchanged.
FROM rust:1.96-slim@sha256:3b05f7c617a200c41c3506097f0d15fc193a1c93bfd8f141007b47cac8f95d3c AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl  ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add "$target"; \
    cargo install cargo-auditable --locked; \
    cargo auditable build --release -p bdinfo-rs --target "$target" --locked; \
    cp "target/$target/release/bdinfo-rs" /bdinfo-rs

# --- runtime: the binary, alone -------------------------------------------
FROM scratch
COPY --from=build /bdinfo-rs /bdinfo-rs
ENTRYPOINT ["/bdinfo-rs"]
