# machined-rs

[![CI](https://github.com/indyjonesnl/machined-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/indyjonesnl/machined-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Talos's `machined`, reimagined in Rust — and decoupled from Kubernetes.**

A single static binary that runs as PID 1 and *is* the operating system's
management layer: it boots the node, configures network/disk/time through a
typed reconcile runtime, supervises a container runtime with real CRI health
checks, brings up your workload only when the runtime is genuinely ready, and
exposes everything over a mutual-TLS gRPC API. No SSH. No shell. No package
manager. One YAML file.

```yaml
# A complete node.
machine:
  hostname: node-1
  install:
    disk: /dev/sda
    wipe: false            # never destructive without explicit consent
  services:
    - id: kubelet          # any payload binary — a Kubernetes kubelet shown
      command: [/usr/bin/rusternetes-kubelet, --node-name, node-1]
      depends_on: [containerd]   # gated on CRI RuntimeReady, not just process-up
      restart: always
```

Operate it remotely with `machinectl` (mutual TLS, client certs issued by the
node's own CA):

```console
$ machinectl get DiskStatus --namespace block
sda   name=sda path=/dev/sda size_bytes=256060514304 model=Samsung_SSD rotational=false read_only=false

$ machinectl get RuntimeStatus
containerd   ready=true name=containerd version=2.0.0

$ machinectl get ServiceStatus
containerd   service_id=containerd state=Running healthy=true message=running
kubelet      service_id=kubelet state=Running healthy=true message=running

$ machinectl reboot
reboot requested

$ machinectl reset          # wipe STATE+EPHEMERAL, reboot, reprovision
reset wipes STATE and EPHEMERAL; pass --yes to confirm
```

## Why

| | machined-rs | Talos Linux | systemd distro |
|---|---|---|---|
| Workload | **any** (k8s distro, or none) | Kubernetes only | anything |
| Management | mTLS gRPC API only | mTLS gRPC API only | SSH + shell |
| Mutable state | two labeled partitions | similar | everywhere |
| Implementation | Rust, one static binary | Go, several services | C + scripts |
| Target | down to 512 MB ARM boards | amd64/arm64 servers | varies |

Talos proved the API-driven immutable node model. machined-rs takes that model,
removes the Kubernetes coupling (the payload is just a supervised service with
a readiness gate), and rebuilds the core in Rust with footprint discipline —
the reference payload is [rusternetes](https://github.com/indyjonesnl/rusternetes),
but nothing in this repo depends on it.

## How it works

```
            ┌────────────────────────── machined (PID 1) ──────────────────────────┐
            │                                                                      │
 machine    │  boot sequencer          typed reconcile runtime         supervisor  │
 config ────┼─→ mount /proc /sys…  ┌──────────────────────────────┐  ┌───────────┐ │
 (YAML)     │   sysctls, hostname  │ network: link/addr/route/DNS │  │containerd │ │
            │   start services ───→│ block: discover→provision→mnt│  │  payload  │ │
            │                      │ time: SNTP sync (periodic)   │  │ (gated on │ │
 machinectl │  mTLS gRPC API       │ runtime: CRI health probe    │  │ CRI ready)│ │
 ──────────→│  get / reboot /      └────────────┬─────────────────┘  └───────────┘ │
   :50000   │  shutdown / reset                 │ typed resource store (CAS,       │
            │                                   ▼ finalizers, owner GC, watch)     │
            └──────────────────────────────────────────────────────────────────────┘
```

The runtime is COSI-style controllers — Kubernetes's reconcile model — but with
a **closed, statically-typed resource set** instead of stringly-typed objects:

```rust
#[async_trait]
pub trait Controller: Send {
    fn name(&self) -> &str;
    fn inputs(&self) -> Vec<Input>;            // watch these resource types
    fn outputs(&self) -> Vec<Output>;          // exclusively own these
    fn resync_interval(&self) -> Option<Duration> { None } // optional timer
    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> Result<()>;
}
```

Adding a resource type is a **compile error** at every place that must handle
it — the API's field mapping has no wildcard arm, on purpose. Every privileged
surface (rtnetlink, GPT partitioning, mount(2), CRI-over-UDS, SNTP,
clock_settime) lives behind a trait with a real and a fake implementation, so
the entire lifecycle is exercised by **160+ root-free tests**; the real
syscall paths have their own privileged tier (`make root-tests`).

Safety is test-pinned, not aspirational: provisioning refuses foreign disks
without `wipe: true`, `reset` can only ever format the two partitions the
guarded provisioner attested (EFI-never is an assertion in the suite), and a
grace-expired service kill is observable in the API rather than silent.

## What works today

| area | status |
|---|---|
| Reconcile runtime (CAS store, finalizers, owner GC, watch, periodic) | ✅ |
| PID 1 boot/shutdown sequencer + service supervisor | ✅ |
| Network: links, addresses, routes, hostname, resolv.conf (rtnetlink) | ✅ |
| Block: discovery → guarded GPT provisioning → mount | ✅ |
| Time: SNTP sync with periodic re-sync | ✅ |
| Node PKI + mutual-TLS gRPC API + `machinectl` | ✅ |
| containerd supervision + CRI health (`RuntimeReady`) | ✅ |
| Payload bring-up gated on runtime readiness | ✅ |
| Graceful stop: SIGTERM→grace→kill (process groups), sync+unmount | ✅ |
| Reset: wipe STATE+EPHEMERAL → reboot → reprovision | ✅ |
| Bootable disk image + QEMU-verified boot (x86_64 + aarch64) | ✅ |
| [aarch64 / Raspberry Pi 3A+ image](docs/raspberry-pi-3a-plus.md): QEMU boot (real SoC model) + manual hardware-verify | ✅ |
| Atomic OS upgrade via in-process kexec (in-memory; M9a) | ✅ |
| Boot from disk via systemd-boot + disk-persistent A/B upgrade, survives a cold reboot (x86; M9b-1) | ✅ |
| [Pi-native A/B upgrade](docs/raspberry-pi-3a-plus.md) (config.txt `os_prefix`; CI: layout + backend, end-to-end hardware-verified) | ✅ |
| upgrade: health-gated auto-rollback to the previous slot (M9b-2) | 🔜 next |
| Streaming logs/events RPCs, per-service health probes | 🔜 planned |

There is an image now: `machined-imager` builds bootable x86_64, aarch64, and
Raspberry Pi 3A+ disk images from pinned Alpine artifacts entirely in userspace
— no root, no loop devices. CI boots the x86_64 and aarch64 images in QEMU and
asserts the mTLS API answers and STATE+EPHEMERAL provision. The x86_64 image
boots **from disk** (UEFI/OVMF → systemd-boot → an A/B slot, no external kernel)
and CI drives a v1→v2 upgrade that stages the new image to the inactive slot,
flips the boot pointer, and **survives a cold reboot** with STATE+PKI intact. The
Pi image boots under `-M raspi3ap` (the
real BCM2837 SoC model) to prove the Pi kernel + machined come up, and its MBR
`/boot` mount path is covered on `-M virt` (QEMU's raspi3 SD model can't expose
the MBR partition table — the on-hardware SD/firmware handoff is verified on a
real Pi, see [docs/raspberry-pi-3a-plus.md](docs/raspberry-pi-3a-plus.md)). The
full lifecycle also runs in the test suite, and every subsystem can be driven
against its fake backend on any Linux machine.

## Build & test

```bash
cargo build --workspace
make pre-commit     # fmt + clippy -D warnings + full test suite (root-free)
make root-tests     # privileged tier: loop devices, netns, clock, real containerd
make boot-test              # x86_64 image → QEMU boot; asserts API + provisioning + live kexec upgrade
make boot-test-aarch64      # aarch64 image → QEMU -M virt boot; asserts API + provisioning
make boot-test-aarch64-rpi  # Pi 3A+ image → QEMU -M raspi3ap (real SoC model); asserts machined boots
make boot-test-aarch64-mbr  # aarch64 MBR image → QEMU -M virt; asserts the Pi's MBR /boot mount path
make build-image-aarch64-rpi   # build the Pi 3A+ SD image (verify on hardware — see docs/raspberry-pi-3a-plus.md)
```

No system `protoc` needed — protobuf codegen uses a vendored binary.

## How it's built

Every subsystem has a committed design spec and implementation plan under
[`docs/superpowers/`](docs/superpowers/) — 17 crates, each milestone
brainstormed, spec'd, reviewed, and merged behind `clippy -D warnings` and a
green suite. Read [`docs/superpowers/specs/`](docs/superpowers/specs/) to see
*why* anything is the way it is before changing it.

Good entry points if you want to contribute:

- **Upgrade rollback (M9b)** — persist the kexec'd image to A/B disk slots + health-gated rollback.
- **Streaming RPCs** — `machinectl logs`/`events` over the existing API.
- **Per-service health probes** — HTTP/exec checks feeding the readiness gate.
- **cgroups** — resource limits for supervised services.

## License

[MIT](LICENSE)
