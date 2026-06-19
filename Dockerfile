# bdinfo-rs is one fully-static, zero-C-dependency musl binary, so the runtime
# image is literally just that binary on `scratch`: no libc, no shell, no OS —
# a ~2 MB image with nothing to patch and no attack surface beyond the binary.
#
# The build stage runs on the *target* platform and builds that arch's musl
# binary natively (the matching system linker handles the static link; Rust adds
# no C of its own). So `docker build` works as-is on amd64 and arm64 hosts, and
# CI gets a multi-arch image by building each arch on its own native runner.

# --- build: compile the static musl binary --------------------------------
FROM rust:1.96-slim AS build
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
    cargo build --release -p bdinfo-rs --target "$target" --locked; \
    cp "target/$target/release/bdinfo-rs" /bdinfo-rs

# --- runtime: the binary, alone -------------------------------------------
FROM scratch
COPY --from=build /bdinfo-rs /bdinfo-rs
ENTRYPOINT ["/bdinfo-rs"]
