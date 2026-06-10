# machined-rs

A generic, immutable Rust node-OS daemon (PID 1 / init + machine management),
inspired by Talos Linux's `machined`. It boots a node, configures it, and
supervises a config-declared workload payload (e.g. a Kubernetes distribution).
Workload- and distro-agnostic; [rusternetes](../rusternetes) is the reference
payload, not a dependency.

## Status

Milestone M0 (the `runtime-core` reconcile foundation) — complete.
See `docs/superpowers/specs/` and `docs/superpowers/plans/`.

## Build

```bash
cargo build --workspace
make pre-commit   # fmt + clippy -D warnings + test
```
