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
LABEL org.opencontainers.image.description="machined-rs CI: Rust + x86_64/aarch64-musl targets + QEMU (x86_64 + aarch64) for the boot-test job"

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      qemu-system-x86 \
      qemu-system-arm \
      qemu-efi-aarch64 \
      ovmf \
      musl-tools \
      gcc-aarch64-linux-gnu \
      libc6-dev-arm64-cross \
      make \
      python3 \
 && rm -rf /var/lib/apt/lists/*

# python3 serves the M9b-1 disk A/B upgrade bundle to the guest: the x86 boot test
# runs `python3 -m http.server` on the host so machined (in the VM) can download
# the v2 bundle over slirp (10.0.2.2), stage it into the inactive A/B slot, flip
# loader.conf, and COLD-reboot into it. No third-party packages — just the stdlib
# http.server.

# ovmf provides the x86_64 UEFI firmware (OVMF_CODE.fd + OVMF_VARS.fd). The
# M9b-1 boot test boots the built image FROM DISK via systemd-boot on its ESP —
# qemu runs OVMF as pflash (no external `-kernel`/`-initrd`), the firmware finds
# the EFI System Partition, loads systemd-boot, which reads loader.conf and boots
# the A/B slot. This is what lets the disk A/B upgrade survive a COLD reboot:
# after machined flips loader.conf's `default` and reboots, OVMF re-reads the
# same disk and systemd-boot picks the inactive (v2) slot.

# qemu-efi-aarch64 provides the AAVMF/edk2 UEFI firmware (QEMU_EFI.fd). aarch64's
# Alpine vmlinuz-virt is an EFI-stub PE kernel — unlike x86's self-decompressing
# bzImage, qemu's bare `-kernel` on `-M virt` can't boot it; the boot test passes
# `-bios QEMU_EFI.fd` so the firmware loads the kernel+initrd from fw_cfg via the
# EFI stub. (No persistent EFI vars needed for direct kernel boot.)

# libc6-dev-arm64-cross provides the aarch64 glibc dev headers/sysroot under
# /usr/aarch64-linux-gnu/include. Without it (and with --no-install-recommends),
# aarch64-linux-gnu-gcc falls back to the host /usr/include and ring's C build
# dies on "bits/libc-header-start.h: No such file or directory". ring only uses
# stdint/stdlib basics, so the glibc aarch64 headers are fine even though rustc
# links the musl libc for the aarch64-unknown-linux-musl target.

# The repo's rust-toolchain.toml pins `channel = "stable"`, so rustup selects
# the `stable` alias inside the checkout — install THAT toolchain (not just the
# image's numbered default) with the musl target and the pinned components, or
# the in-repo build fails with "can't find crate for std" for musl.
RUN rustup toolchain install stable --profile minimal \
      --component rustfmt --component clippy \
      --target x86_64-unknown-linux-musl \
      --target aarch64-unknown-linux-musl \
 && rustup default stable

# Sanity: fail the image build if any tool the boot-test relies on is missing.
RUN qemu-system-x86_64 --version \
 && qemu-system-aarch64 --version \
 && test -f /usr/share/OVMF/OVMF_CODE.fd \
 && test -f /usr/share/OVMF/OVMF_VARS.fd \
 && musl-gcc --version \
 && aarch64-linux-gnu-gcc --version \
 && python3 --version \
 && rustup target list --installed | grep -qx x86_64-unknown-linux-musl \
 && rustup target list --installed | grep -qx aarch64-unknown-linux-musl
