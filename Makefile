.PHONY: pre-commit fmt clippy test root-tests

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
