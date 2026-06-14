# M8 — Run an Actual Pod (design)

**Date:** 2026-06-14
**Status:** Approved (design); decomposed into M8a + M8b
**Builds on:** M7b (containerd 2.0.9 + runc 1.4.3 on /boot/bin, v3 CRI config, RuntimeReady asserted in CI).

## Goal

Make `RuntimeReady` *mean something*: machined itself runs a container via CRI and reports
`PodStatus=Running`, with CI asserting a real workload runs inside the booted node. machined
becomes a minimal pod orchestrator over CRI — the core of what a kubelet does — not just a
runtime supervisor.

## Non-negotiable constraints (pluggability)

CRI and CNI are both universal standards. The whole point of this milestone is to use them as
such — containerd and the bridge plugin are *defaults we ship*, never assumptions baked into
machined's core. Two guardrails, enforced in review:

1. **CRI stays pluggable.** The `PodController` depends ONLY on the `CriClient` **trait** (CRI v1
   proto: RunPodSandbox / CreateContainer / StartContainer / …). It has **no containerd
   dependency** and reaches no containerd-native (ttrpc) API. The CRI endpoint (socket path) is
   config. Any CRI-conformant runtime (CRI-O, …) drops in by changing the socket + the runtime
   service config. All containerd-specific knobs (snapshotter, `sandbox_image`, BinaryName)
   stay confined to `crates/config/src/runtime_svc.rs::containerd_config_toml` — the runtime
   service config generator — never leaking into the controller.

2. **CNI stays pluggable.** machined treats CNI as the standard contract: a `bin_dir` of plugin
   binaries + a `conf_dir` conflist, with containerd's CRI exec-ing the plugins. machined's
   ONLY job is to **stage plugin binaries + a conflist and point containerd at the dirs**. It
   writes **no netlink and no iptables** for pod networking — the `bridge` plugin creates the
   bridge, `host-local` does IPAM, `firewall`/`portmap` do iptables. The bridge conflist is a
   swappable default; flannel/calico drop in by swapping the binary + conflist with zero
   machined code change.

3. **Config schema is CRI/CNI-shaped, not vendor-shaped.** A pod is declared as (name, image,
   command, args, host-network bool); the network is referenced by CNI network name / conflist —
   never a `bridge:` struct or containerd-specific fields.

## Decomposition

| | Scope | Network | Proves |
|---|---|---|---|
| **M8a** | machined runs a pod end-to-end via CRI | **host** (pod shares node netns, no CNI) | a container runs under real delegated cgroups; `PodStatus=Running` asserted in CI |
| **M8b** | CNI bridge networking | **bridge** (veth + per-pod IP via CNI plugins) | a pod gets a non-host address; CI asserts it |

M8a is the core. M8b adds real pod networking. CNI is not dropped — it is M8b. Each gets its
own spec→plan→build cycle; this doc details M8a and sketches M8b.

---

## M8a — machined runs a pod

### 1. CRI client (`crates/cri`)

Extend the trimmed proto + `CriClient` trait from the health subset (Version/Status) to the
pod lifecycle. RPCs to add (RuntimeService unless noted):

- `RunPodSandbox` / `StopPodSandbox` / `RemovePodSandbox` / `PodSandboxStatus`
- `CreateContainer` / `StartContainer` / `StopContainer` / `RemoveContainer` / `ContainerStatus`
- `ImageStatus` / `PullImage` (ImageService) — for M8a images are **pre-imported** (see §4), so
  the controller uses `ImageStatus` to confirm presence and only `PullImage` as a fallback.

Vendor the needed CRI v1 message subset (`PodSandboxConfig`, `ContainerConfig`, `ImageSpec`,
`LinuxPodSandboxConfig` with `NamespaceOption.network = NODE` for host-net, etc.) into
`crates/cri/proto/runtime.proto`. Grow the `CriClient` trait with async methods mirroring these
RPCs; keep the existing `version()`/`ready()`. Provide a **fake** `CriClient` for controller
unit tests (in-memory sandbox/container state machine), matching the existing trait/real/fake
pattern.

The real client stays a thin gRPC wrapper over the CRI socket — no containerd specifics.

### 2. PodController (`crates/controllers`)

A new closed-enum reconcile controller, following the existing controller pattern:

- **Input:** the desired pods from node config (§3), surfaced as a `PodSpec` resource.
- **Gate:** reconciles only once `RuntimeStatus.ready == true` (reuse the existing
  `RuntimeReadiness` gate — same mechanism that gates containerd consumers today).
- **Reconcile, per pod:** ensure image present (`ImageStatus`; pull only if absent) →
  `RunPodSandbox` (host network for M8a) → `CreateContainer` → `StartContainer` →
  `ContainerStatus` until `Running`.
- **Output:** publishes a `PodStatus` resource (namespace `runtime`): `name`, `phase`
  (`Pending|Running|Failed`), `container_id`, `message`. Idempotent — re-reconcile observes
  existing sandbox/container and does not recreate.

Depends only on the `CriClient` trait + the resource store. No containerd import.

### 3. Config schema (`crates/config`)

Add a `pods` list to the machine config (CRI-shaped):

```yaml
machine:
  pods:
    - name: hello
      image: docker.io/library/busybox:1.36   # ref of a pre-baked image (§4)
      command: ["/bin/sh", "-c"]
      args: ["echo machined-pod-ok; sleep 3600"]
      host_network: true        # M8a: always true (CNI is M8b)
```

`PodConfig { name, image, command, args, host_network }`. No vendor fields. M8a ignores any
non-host-network value (documented); M8b honours it.

### 4. Images — pre-baked OCI archives (hermetic)

No CI registry egress. Pin two images as sha256-pinned OCI archives in
`crates/imager/artifacts.toml` (amd64 only — x86 is the only pod-run arch):

- **pause** (`registry.k8s.io/pause:3.10`) — the sandbox infra container.
- **busybox** (`docker.io/library/busybox:1.36`) — the test workload.

New imager artifact **kind `oci-image`** → staged to `/boot/images/<name>.tar` (an
`ctr images export` / OCI-layout archive). At boot, the images are imported into containerd's
`k8s.io` namespace before the PodController runs. Import mechanism: a small machined boot step
(or the PodController's image-ensure path) shells `ctr -n k8s.io images import
/boot/images/<name>.tar` (`ctr` already ships in the containerd boot-tarball on /boot/bin).
`sandbox_image` in `containerd_config_toml` is set to the pinned pause ref so the CRI sandbox
uses the pre-baked pause (no pull).

> Build-time: producing the OCI archives (pull-once → export) happens on a networked machine and
> the artifacts are pinned by sha, same as every other artifact. The CI/boot path is offline.

### 5. cgroup-v2 controller delegation (`crates/platform` + `crates/sequencer`)

The original M8 driver. cgroup2 is already mounted (M7b-2) but no controllers are delegated and
PID1 sits in the root cgroup — containers can't get cgroups (cgroup-v2 "no internal processes"
rule). Add an early-boot step (a new sequencer task in the `early`/post-mount phase, before
`StartServices`):

- Move PID1 (machined) into a leaf cgroup: create `/sys/fs/cgroup/init.scope`, write machined's
  pid to its `cgroup.procs`. (containerd, started as a service child, inherits / is likewise
  placed off the root.)
- Delegate controllers to the root subtree: write `+cpu +memory +pids +io` to
  `/sys/fs/cgroup/cgroup.subtree_control`.

Pure helper (compute the writes) + a thin syscall-side, fake-tested, matching the platform
trait pattern. **Verified empirically by the pod actually running** — if delegation is wrong,
`StartContainer` fails and CI catches it.

### 6. Kernel modules (`crates/imager/src/modules.rs`)

containerd's default **overlayfs** snapshotter needs `overlay.ko`. Add `overlay` to the module
set used by the qemu-virt images (x86_64). (veth/bridge/br_netfilter are **M8b**, not added
here.) Confirm at plan time whether `overlay` is `=m` or builtin in Alpine `linux-virt`; add to
the module list only if `=m`.

### 7. containerd config (`crates/config/src/runtime_svc.rs`)

Containerd-specific changes live HERE only (quarantine):

- Keep the default `overlayfs` snapshotter (now backed by `overlay.ko`).
- Set `sandbox_image = "<pinned pause ref>"`.

No other consumer learns these values.

### 8. CI assertion (`scripts/boot-test-x86_64.sh`, x86 only)

After the existing RuntimeReady gate, add a stage: poll `machinectl get PodStatus --namespace
runtime` until the `hello` pod shows `phase=Running` (with the container id present). Reuse the
existing `ctl()` helper + deadline-loop structure. aarch64 + Pi keep their current bars
(RuntimeReady / build-only) — pod-run is x86 (KVM-fast) only; pre-baked images are amd64.

`examples/node-ci.yaml` gains the `pods:` entry from §3. The aarch64 / Pi configs do **not**
(no pod-run there).

### 9. Out of scope for M8a

CNI / bridge networking (M8b); image pull from a registry; multi-container pods; pod restart
policy / health probes; aarch64 pod-run; persistent pod state across reboot.

---

## M8b — CNI bridge networking (sketch)

- **Modules:** add `veth`, `bridge`, `br_netfilter`, and the iptables modules to the x86 module
  set.
- **Plugins:** pin the containernetworking `bridge` + `host-local` + `loopback` + `portmap`
  plugins as artifacts, staged to a CNI `bin_dir` (e.g. `/boot/cni/bin`).
- **Conflist:** stage a default bridge conflist to a `conf_dir` (e.g. `/boot/cni/conf`). This is
  a **swappable default** — no bridge logic in Rust.
- **containerd:** point the CRI plugin's `bin_dir`/`conf_dir` at the staged dirs (in
  `runtime_svc.rs`, the quarantine zone).
- **Config/controller:** `host_network: false` now honoured; the pod gets a veth + an IP from
  host-local IPAM via the CNI plugins. machined writes **no netlink, no iptables**.
- **CI:** assert the pod's `PodStatus` carries a non-host IP (and optionally that it can reach a
  peer). x86 only.

---

## Risks / watch-outs

- **CRI proto surface.** The pod-lifecycle subset is sizeable; vendor only the messages the
  controller uses, keep the trait minimal. Risk = trait bloat / accidental containerd coupling —
  caught by the pluggability guardrail in review.
- **cgroup delegation correctness.** The leaf-move + subtree_control ordering is fiddly; the
  pod-run integration test is the real proof (fakes can't validate it). Generous boot-test
  deadline (containerd may restart once or twice — M7b-1 backoff is 1s→30s; confirm the deadline
  headroom now that a pod-run stage is appended).
- **overlay snapshotter on a tiny initramfs.** overlayfs over the ext4 EPHEMERAL volume should
  be fine; confirm containerd's snapshot root (`/var/lib/containerd`) lands on EPHEMERAL (see
  M7b carry-forward #8 — root-vs-EPHEMERAL ordering) before the PodController runs.
- **Image size.** pause (~700KB) + busybox (~4MB) on the FAT — negligible, but they enlarge the
  initramfs/boot. Acceptable.
