# machined-rs

A generic, immutable Rust node-OS daemon (PID 1 / init + machine management),
inspired by Talos Linux's `machined`. It boots a node, configures it, and
supervises a config-declared workload payload (e.g. a Kubernetes distribution).
Workload- and distro-agnostic; [rusternetes](../rusternetes) is the reference
payload, not a dependency.

## Status

Milestone M0 (the `runtime-core` reconcile foundation) — complete.
See `docs/superpowers/specs/` and `docs/superpowers/plans/`.

### Running a payload

machined supervises an external containerd and health-checks it over CRI. Any
config-declared service with `depends_on: [containerd]` starts only once the
runtime is genuinely ready (process up **and** CRI `RuntimeReady`). See
`docs/examples/node-with-kubelet.yaml` for a Kubernetes-payload example.

## Build

```bash
cargo build --workspace
make pre-commit   # fmt + clippy -D warnings + test
```

## Privileged tests

`make root-tests` runs the `#[ignore]`d privileged integration tests for
`machined-platform`, `machined-block`, `machined-netlink`, `machined-time`,
and `machined-cri` under `sudo -E`. Requirements: a Linux host, passwordless
or interactive sudo, and loop-device support; the containerd CRI test
additionally needs a running containerd at
`/run/containerd/containerd.sock`.
