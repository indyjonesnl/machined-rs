.PHONY: pre-commit fmt clippy test root-tests dist-x86_64 dist-aarch64 boot-test

pre-commit: fmt clippy test

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --workspace

# Privileged test tier: loop devices, netns, clock_settime, real containerd.
# Run on a Linux host with sudo; containerd test additionally needs a running
# containerd at /run/containerd/containerd.sock.
root-tests:
	sudo -E cargo test -p machined-platform -p machined-block -p machined-netlink -p machined-time -p machined-cri -- --ignored

# Static machined for images (vendored protoc; needs a musl C toolchain for the
# ring/aws-lc-rs build scripts). CI has musl-tools (musl-gcc), so the plain build
# works there. On a host WITHOUT musl-tools the plain build fails linking the C
# bits, so fall back to the system gcc with the musl target env overrides
# (-U_FORTIFY_SOURCE because glibc's gcc injects fortify symbols musl lacks).
dist-x86_64:
	@if command -v musl-gcc >/dev/null 2>&1; then \
		cargo build --release --target x86_64-unknown-linux-musl -p machined; \
	elif cargo build --release --target x86_64-unknown-linux-musl -p machined; then \
		:; \
	else \
		echo "musl-gcc absent and plain build failed; retrying with gcc override"; \
		CC_x86_64_unknown_linux_musl=gcc \
		CFLAGS_x86_64_unknown_linux_musl="-U_FORTIFY_SOURCE" \
		cargo build --release --target x86_64-unknown-linux-musl -p machined; \
	fi

# Static machined for aarch64 images (cross-linked with the gnu aarch64 gcc;
# rustc supplies the musl libc, so no musl sysroot needed — ring builds with
# aarch64-linux-gnu-gcc as CC).
dist-aarch64:
	@if command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then \
		CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
		CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
		AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
		cargo build --release --target aarch64-unknown-linux-musl -p machined; \
	else \
		echo "FATAL: aarch64-linux-gnu-gcc not found (apt install gcc-aarch64-linux-gnu)"; \
		exit 1; \
	fi

# Build the x86_64 image + boot it in QEMU, assert the node comes up.
boot-test: dist-x86_64
	cargo build --release -p machined-imager -p machinectl
	./scripts/boot-test-x86_64.sh
