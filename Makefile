.PHONY: pre-commit fmt clippy test root-tests dist-x86_64 boot-test

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

# Build the x86_64 image + boot it in QEMU, assert the node comes up.
boot-test: dist-x86_64
	cargo build --release -p machined-imager -p machinectl
	./scripts/boot-test-x86_64.sh
