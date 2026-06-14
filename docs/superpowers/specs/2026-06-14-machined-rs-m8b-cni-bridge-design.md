# M8b — CNI Bridge Networking (design)

**Date:** 2026-06-14
**Status:** Approved (design)
**Builds on:** M8a (machined runs a pod via CRI; host-network only). M8b gives a pod a real bridge IP.

## Goal

A pod declared with `host_network: false` gets a real per-pod IP from CNI bridge networking
(veth + bridge + host-local IPAM), with egress NAT (ipMasq). The x86_64 boot test asserts the
pod reaches `Running` AND carries a non-host bridge IP. This is the "CNI" half of the user's
M8 networking choice (overlay + CNI bridge); M8a delivered overlay + host-net.

## Non-negotiable constraint (CNI pluggability)

CNI is a universal standard: a `bin_dir` of plugin binaries + a `conf_dir` conflist, with
containerd's CRI exec-ing the plugins per the spec. machined's ONLY job is to **stage the plugin
binaries + a conflist and point containerd at the dirs**. machined writes **no netlink and no
iptables/nftables** for pod networking — the `bridge` plugin creates the bridge + veth, the
`host-local` plugin does IPAM, and (with `ipMasq`) the bridge plugin programs nftables itself.
The bridge conflist is a **swappable default**: flannel/calico drop in by replacing the binaries +
conflist with zero machined code change. Enforced in review:
`grep -rn "netlink\|iptables\|nftables\|nft_" crates/machined crates/controllers` finds nothing
CNI-related (the existing `machined_netlink` link/address controllers are unrelated node-NIC
config, not pod networking).

## Key design decision: nftables ipMasq backend (no iptables binary)

`ipMasq` (egress NAT for the pod) normally drives the bridge plugin's `go-iptables`, which execs
a userspace `iptables` binary — which the immutable initramfs does not ship. containernetworking
plugins **v1.4.0+** add `ipMasqBackend: "nftables"`: the bridge plugin programs masquerade via
**in-process nftables netlink**, needing only kernel modules, **no userspace binary**. M8b pins
plugins ≥v1.4 and sets `ipMasqBackend: "nftables"` in the conflist. This keeps M8b a single
milestone (no apk-staging an iptables/nftables userspace toolchain).

## Honest CI scope

The boot test asserts the pod is `Running` and has a non-host bridge IP — proof that CNI ran and
host-local assigned an address. **Egress (ipMasq) is configured but NOT CI-verified**: confirming
the pod can actually reach off-node needs running a command inside the pod (a CRI `ExecSync` path
machined doesn't have) and surfacing the result. ipMasq is included for production realism; the
asserted bar is IP assignment. This is stated so the green boot test isn't mistaken for an
egress-connectivity proof. (A future `ExecSync`-based egress assertion is a follow-up.)

## Scope: x86_64 only

Like M8a's pod-run, M8b's CNI networking is exercised on the x86_64 boot test only (KVM-fast).
aarch64 keeps the RuntimeReady bar; the Pi stays build-only. CNI plugins are pinned amd64-only.

---

## Components

### 1. CRI client — `pod_ip` via PodSandboxStatus (`crates/cri`)

M8a deliberately skipped `PodSandboxStatus`. Add it: vendor the `PodSandboxStatusRequest` /
`PodSandboxStatusResponse` / `PodSandboxStatus` / `PodSandboxNetworkStatus` messages (field
numbers from cri-api v1), add a trait method:

```rust
/// The sandbox's assigned IP (CRI PodSandboxStatus → status.network.ip).
/// None when the sandbox has no network status yet / host-network (empty ip).
async fn pod_ip(&self, sandbox_id: &str) -> Result<Option<String>>;
```

`GrpcCriClient` calls `pod_sandbox_status` and returns `status.network.ip` (None if empty).
`FakeCriClient` grows a per-sandbox optional ip (seedable, e.g. `with_sandbox_ip`) so the
PodController test can assert pod_ip propagation. This is the only CRI surface M8b adds.

### 2. `PodStatus.pod_ip` (`crates/resources` + `crates/apiserver`)

Add `pub pod_ip: String` to the `PodStatus` spec. The apiserver mapping renders it as `pod_ip=…`
(after `container_id`). For a host-network pod the field is the node IP or empty; for a CNI pod
it's the bridge IP. Every `PodStatus { ... }` literal across the codebase gains the field (the
M8a tests + the controller).

### 3. PodController reads the IP (`crates/controllers/src/runtime/pod.rs`)

After `ensure_sandbox` returns a sandbox id and the container is observed `Running`, call
`cri.pod_ip(&sandbox)` and populate `PodStatus.pod_ip` (empty on None/error — best-effort, the
5s resync refills it once CNI finishes). The controller stays CRI-trait-only; it never learns
"bridge" exists.

### 4. Kernel modules (`crates/imager/src/modules.rs`)

Add to `VIRT_MODULES`: `veth`, `bridge`, `br_netfilter`, `nf_tables`, `nf_nat`, `nf_conntrack`.
(`overlay` is already there from M8a.) Verified `=m` against the real Alpine linux-virt
`modules.dep` at plan time (the overlay precedent: a real imager build resolves the closure; if
any is built-in `=y` it's dropped from the list, if `=m` it's kept). Shared x86_64/aarch64 —
aarch64 loads them at boot harmlessly (it doesn't run CNI pods), confirmed by its boot test.

### 5. CNI plugins — new `cni-plugins` imager artifact kind (`crates/imager`)

Pin `cni-plugins-linux-amd64-v1.x.tgz` (≥1.4.0) in `artifacts.toml` (x86_64). A new artifact
kind `cni-plugins` extracts the (flat) plugin binaries from the tgz into a CNI bin staging dir →
FAT `/cni/bin/` → guest `/boot/cni/bin/`. M8b only needs `bridge`, `host-local`, `loopback`;
the extractor may stage all binaries in the tarball (they're small) or filter to the three —
filtering to the three keeps the image lean. `chmod 0755`, path-escape guarded (same posture as
`extract_boot_tarball`).

### 6. Bridge conflist — static asset staged by the imager

A committed default conflist (e.g. `crates/imager/assets/10-machined-bridge.conflist`) staged by
the imager to FAT `/cni/conf/` → guest `/boot/cni/conf/`. Contents:

```json
{
  "cniVersion": "1.0.0",
  "name": "machined-bridge",
  "plugins": [
    {
      "type": "bridge",
      "bridge": "cni0",
      "isGateway": true,
      "ipMasq": true,
      "ipMasqBackend": "nftables",
      "hairpinMode": true,
      "ipam": {
        "type": "host-local",
        "ranges": [[{ "subnet": "10.88.0.0/16" }]],
        "routes": [{ "dst": "0.0.0.0/0" }]
      }
    },
    { "type": "loopback" }
  ]
}
```

This is the swappable default — machined treats it as opaque bytes it stages. (The imager stages
it; it is NOT generated by machined, keeping machined CNI-agnostic.)

### 7. containerd CNI config (quarantine, `crates/config/src/runtime_svc.rs`)

The containerd CRI plugin's CNI dirs are containerd-specific → set ONLY here. Add under
`[plugins.'io.containerd.cri.v1.runtime']`:

```toml
[plugins.'io.containerd.cri.v1.runtime'.cni]
  bin_dir = "/boot/cni/bin"
  conf_dir = "/boot/cni/conf"
```

(The exact v3 CNI sub-key is confirmed at plan time against containerd 2.0.9; the generated TOML
must still parse + keep the existing M8a assertions.)

### 8. `net.bridge.bridge-nf-call-iptables` sysctl

The k8s-standard `net.bridge.bridge-nf-call-iptables=1` (so bridged pod traffic traverses the
netfilter hooks) is set via the **existing generic node-config `sysctls`** mechanism — not a
CNI-specific machined code path. Added to `examples/node-ci.yaml`'s `sysctls`. It depends on
`br_netfilter` being loaded first (module load happens in PID1 early boot, before services /
the sysctl task), so the sysctl key exists when applied. (If ordering proves racy, the sysctl
task tolerates a missing key with a warning — confirm at plan time.)

### 9. node-ci.yaml — add `netpod`

Keep the M8a `hello` pod (`host_network: true`). Add:

```yaml
    - name: netpod
      image: docker.io/library/busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["echo machined-netpod-ok; sleep 3600"]
      host_network: false
```

`netpod` reuses the already-pre-baked busybox image (no new oci-image artifact). Add the bridge-nf
sysctl to the same file.

### 10. boot-test assertion (`scripts/boot-test-x86_64.sh`, x86 only)

After the existing `hello phase=Running` gate, add: poll `PodStatus` until `netpod` shows
`phase=Running` AND a `pod_ip=` that is non-empty and not the node address `10.0.2.15`
(i.e. matches `pod_ip=10\.88\.` — the bridge subnet). Reuse the `ctl()` helper + deadline-loop
structure. Generous deadline (CNI plugin exec + IPAM on first pod start).

### 11. Out of scope

Egress connectivity assertion (needs CRI ExecSync); pod-to-pod connectivity; NetworkPolicy;
multiple CNI networks; IPv6; aarch64/Pi CNI; host-port mapping (portmap) + firewall isolation
plugins; persistent IPAM state across reboot (host-local's /var store — ephemeral is fine).

---

## Operator dependency

Like M8a's images, the CNI plugin tarball is a pinned artifact: `cni-plugins-linux-amd64-v1.x.tgz`
from the containernetworking/plugins GitHub releases, pinned by url + sha256 in `artifacts.toml`.
Unlike the OCI images, this is a public GitHub release asset (same shape as the containerd/runc
artifacts already pinned), so it needs no operator hosting — the implementer pins it directly.
(The M8a pause/busybox OCI hosting remains the separate outstanding M8a operator step;
`netpod` reuses busybox, so M8b's green boot test still depends on that M8a hosting being done.)

## Risks / watch-outs

- **Module closure**: `nf_tables`/`nf_nat`/`nf_conntrack`/`br_netfilter`/`bridge`/`veth` must
  resolve `=m` in linux-virt (verify with a real build, overlay-style). `nf_tables` pulls a
  dep subtree — the closure resolver handles deps, but confirm none are `=y`-only.
- **nftables ipMasq under the virt kernel**: the bridge plugin's nftables backend needs
  `nf_tables` + nat support; if the masquerade silently no-ops the IP still assigns (the asserted
  bar holds) — egress is unverified anyway, so a masquerade gap won't fail CI but should be noted.
- **containerd CNI conf_dir readiness**: containerd reads `conf_dir` at CRI-plugin load; the
  conflist must be staged on /boot (it is, by the imager) before containerd starts. /boot is
  mounted ro early — fine (containerd only reads it).
- **First-pod CNI latency**: plugin exec + IPAM adds seconds to `netpod` start; bump the boot-test
  pod deadline.
- **busybox/pause hosting (M8a carry-over)**: M8b's boot test can't go green until the M8a OCI
  images are hosted + pinned. Flag in the final handoff.
