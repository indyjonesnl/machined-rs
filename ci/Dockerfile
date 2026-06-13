# CI tool image for the machined-rs boot-test job.
#
# Bakes the Rust toolchain (+ the static musl target), QEMU, and musl-tools so
# the boot-test job skips per-run `rustup target add` and `apt-get install
# qemu-system-x86 musl-tools`. Built + pushed to GHCR by
# .github/workflows/ci-image.yml on Dockerfile changes and weekly (to pick up
# base-image security updates).
#
# Why no protoc: the workspace's protobuf codegen uses protoc-bin-vendored, so
# no system protoc is needed.
FROM rust:1-bookworm

LABEL org.opencontainers.image.source=https://github.com/indyjonesnl/machined-rs
LABEL org.opencontainers.image.description="machined-rs CI: Rust + x86_64-musl target + QEMU for the boot-test job"

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      qemu-system-x86 \
      musl-tools \
      make \
 && rm -rf /var/lib/apt/lists/*

# The repo's rust-toolchain.toml pins `channel = "stable"`, so rustup selects
# the `stable` alias inside the checkout — install THAT toolchain (not just the
# image's numbered default) with the musl target and the pinned components, or
# the in-repo build fails with "can't find crate for std" for musl.
RUN rustup toolchain install stable --profile minimal \
      --component rustfmt --component clippy \
      --target x86_64-unknown-linux-musl \
 && rustup default stable

# Sanity: fail the image build if any tool the boot-test relies on is missing.
RUN qemu-system-x86_64 --version \
 && musl-gcc --version \
 && rustup target list --installed | grep -qx x86_64-unknown-linux-musl
