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

# Static machined for images (vendored protoc; needs musl-tools).
dist-x86_64:
	cargo build --release --target x86_64-unknown-linux-musl -p machined

# Build the x86_64 image + boot it in QEMU, assert the node comes up.
boot-test: dist-x86_64
	cargo build --release -p machined-imager -p machinectl
	./scripts/boot-test-x86_64.sh
