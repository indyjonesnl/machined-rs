#!/usr/bin/env bash
# Boot the x86_64 image in QEMU and assert the node comes up end-to-end:
#   - the mTLS management API answers (machinectl version)
#   - STATE + EPHEMERAL volumes are provisioned (CompleteLayout ran)
#
# Binaries are resolved via $CARGO_TARGET_DIR (this host redirects cargo output
# out of ./target), but the work dir stays under the repo's ./target so image
# artifacts, the imager cache, and CI paths remain stable and predictable.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET_DIR=${CARGO_TARGET_DIR:-target}
WORK=target/boot-test
IMG=$WORK/machined-x86_64.img
SERIAL=$WORK/serial.log
MACHINED=$TARGET_DIR/x86_64-unknown-linux-musl/release/machined
IMAGER=$TARGET_DIR/release/machined-imager
CTL=$TARGET_DIR/release/machinectl
TIMEOUT=${BOOT_TEST_TIMEOUT:-240}
# Port 50000 may be busy on a dev host; override with BOOT_TEST_PORT.
PORT=${BOOT_TEST_PORT:-50000}

command -v qemu-system-x86_64 >/dev/null || { echo "FATAL: qemu-system-x86_64 not installed"; exit 2; }
[ -x "$MACHINED" ] || { echo "FATAL: $MACHINED missing — run make dist-x86_64"; exit 2; }
[ -x "$IMAGER" ]   || { echo "FATAL: $IMAGER missing — run cargo build --release -p machined-imager"; exit 2; }
[ -x "$CTL" ]      || { echo "FATAL: $CTL missing — run cargo build --release -p machinectl"; exit 2; }

rm -rf "$WORK"; mkdir -p "$WORK"

# Pre-baked node PKI: CA + server identity + machinectl client bundle under pki/machinectl/.
"$IMAGER" gen-pki --out "$WORK/pki"
# --cache is pinned (the imager default is repo-relative target/imager-cache anyway,
# but pin it so the cached Alpine artifacts are reused deterministically).
"$IMAGER" build --arch x86_64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --image-id v1 \
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

# v2: same inputs, different image-id → a kexec target with a flipped marker.
"$IMAGER" build --arch x86_64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --image-id v2 \
  --out "$WORK/machined-v2.img" --emit-boot "$WORK/boot-v2" --cache target/imager-cache
tar -czf "$WORK/bundle.tgz" -C "$WORK/boot-v2" vmlinuz initramfs.img
BUNDLE_SHA=$(sha256sum "$WORK/bundle.tgz" | cut -d' ' -f1)

echo "image:     $IMG ($(du -h "$IMG" | cut -f1))"
echo "vmlinuz:   $WORK/boot/vmlinuz ($(du -h "$WORK/boot/vmlinuz" | cut -f1))"
echo "initramfs: $WORK/boot/initramfs.img ($(du -h "$WORK/boot/initramfs.img" | cut -f1))"

KVM_FLAG=""
[ -w /dev/kvm ] && KVM_FLAG="-enable-kvm -cpu host"

# shellcheck disable=SC2086  # KVM_FLAG is an intentional word-split flag list.
qemu-system-x86_64 $KVM_FLAG -m 512 -machine q35 \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -append "console=ttyS0" \
  -drive file="$IMG",if=virtio,format=raw \
  -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${PORT}-:50000" \
  -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$SERIAL" &
QEMU=$!

# Serve the v2 upgrade bundle to the guest over QEMU slirp (guest sees host at 10.0.2.2).
UP_PORT=${UPGRADE_HTTP_PORT:-18080}
( cd "$WORK" && python3 -m http.server "$UP_PORT" --bind 0.0.0.0 >/dev/null 2>&1 ) &
HTTPD=$!
trap 'kill $QEMU $HTTPD 2>/dev/null || true; wait $QEMU $HTTPD 2>/dev/null || true' EXIT

# Hard-cap every CLI call: the client has its own connect/request timeouts, but
# this is a belt-and-suspenders bound so a single call can never wedge the loop.
ctl() { timeout 15 "$CTL" --bundle "$WORK/pki/machinectl" --endpoint "https://127.0.0.1:${PORT}" "$@"; }

echo "waiting for API (max ${TIMEOUT}s)..."
deadline=$((SECONDS + TIMEOUT))
while [ $SECONDS -lt $deadline ]; do
  if ctl version >/dev/null 2>&1; then break; fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -50 "$SERIAL"; exit 1; fi
  sleep 1
done
if [ $SECONDS -ge $deadline ]; then echo "TIMEOUT waiting for API"; tail -80 "$SERIAL"; exit 1; fi
echo "API up: $(ctl version)"

# machinectl `get` prints one row per resource: "<id>\t<k=v k=v ...>".
# A provisioned VolumeStatus row looks like:
#   STATE     name=STATE device=/dev/vda2 fs=ext4 label=STATE phase=Provisioned
# Assert both managed volumes are present AND Provisioned (CompleteLayout ran).
echo "checking provisioned volumes (namespace block)..."
vol_deadline=$((SECONDS + 120))
volumes_ok=0
while [ $SECONDS -lt $vol_deadline ]; do
  VOLS=$(ctl get VolumeStatus --namespace block 2>/dev/null || true)
  if echo "$VOLS" | grep -Eq 'name=STATE .*phase=Provisioned' \
     && echo "$VOLS" | grep -Eq 'name=EPHEMERAL .*phase=Provisioned'; then
    echo "$VOLS"; volumes_ok=1; break
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -80 "$SERIAL"; exit 1; fi
  sleep 2
done
if [ "$volumes_ok" -ne 1 ]; then
  echo "volumes never provisioned"; tail -80 "$SERIAL"; exit 1
fi

echo "checking runtime readiness (namespace runtime)..."
rt_deadline=$((SECONDS + 120))
runtime_ok=0
while [ $SECONDS -lt $rt_deadline ]; do
  RT=$(ctl get RuntimeStatus --namespace runtime 2>/dev/null || true)
  if echo "$RT" | grep -Eq 'ready=true'; then
    echo "$RT"; runtime_ok=1; break
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
if [ "$runtime_ok" -ne 1 ]; then
  echo "runtime never became ready:"; ctl get RuntimeStatus --namespace runtime || true
  tail -120 "$SERIAL"; exit 1
fi

# The PodController pulls the pre-baked busybox pod up via CRI. A running pod row:
#   hello  name=hello phase=Running container_id=... message=
echo "checking host-net pod is Running (namespace runtime)..."
pod_deadline=$((SECONDS + 60))
hello_ok=0
while [ $SECONDS -lt $pod_deadline ]; do
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -Eq 'name=hello .*phase=Running'; then
    echo "$PODS"; hello_ok=1; break
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
if [ "${hello_ok:-0}" -ne 1 ]; then
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -q 'message=image not present'; then
    echo "SKIP: pod gate — pre-baked OCI images not hosted (M8a operator step); proceeding to the M9a upgrade proof"
    echo "$PODS"
  else
    echo "hello pod failed for a non-image reason:"; echo "$PODS"; tail -200 "$SERIAL"; exit 1
  fi
fi

# netpod is host_network:false → CNI bridge assigns it a 10.88.x address.
# A running CNI pod row: netpod  name=netpod phase=Running container_id=... pod_ip=10.88.0.x message=
echo "checking CNI pod has a bridge IP (namespace runtime)..."
net_deadline=$((SECONDS + 60))
while [ $SECONDS -lt $net_deadline ]; do
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -Eq 'name=netpod .*phase=Running .*pod_ip=10\.88\.'; then
    echo "$PODS"; pods_ok=1; break
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
if [ "${pods_ok:-0}" -ne 1 ]; then
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -q 'message=image not present'; then
    echo "SKIP: CNI pod gate — pre-baked OCI images not hosted (M8a operator step); proceeding to the M9a upgrade proof"
    echo "$PODS"
  else
    echo "netpod failed for a non-image reason:"; echo "$PODS"; tail -200 "$SERIAL"; exit 1
  fi
fi

# --- M9a: kexec upgrade v1 -> v2 ---
echo "asserting image-id v1..."
V1=$(ctl version 2>/dev/null || true)
echo "version: $V1"
echo "$V1" | grep -q 'image_id=v1' || { echo "expected image_id=v1, got: $V1"; tail -120 "$SERIAL"; exit 1; }

echo "triggering upgrade to v2 (http://10.0.2.2:${UP_PORT}/bundle.tgz)..."
ctl upgrade "http://10.0.2.2:${UP_PORT}/bundle.tgz" "$BUNDLE_SHA" || { echo "upgrade RPC failed"; tail -120 "$SERIAL"; exit 1; }

echo "waiting for the node to kexec into v2 (max 240s)..."
up_deadline=$((SECONDS + 240))
while [ $SECONDS -lt $up_deadline ]; do
  V=$(ctl version 2>/dev/null || true)
  if echo "$V" | grep -q 'image_id=v2'; then
    echo "post-upgrade version: $V"
    # STATE/PKI persisted across the warm boot: the SAME machinectl bundle still
    # authenticates (we are still using it), and volumes are still Provisioned.
    VOLS=$(ctl get VolumeStatus --namespace block 2>/dev/null || true)
    if echo "$VOLS" | grep -Eq 'name=STATE .*phase=Provisioned' \
       && echo "$VOLS" | grep -Eq 'name=EPHEMERAL .*phase=Provisioned'; then
      echo "$VOLS"; echo "BOOT TEST PASSED (kexec upgrade v1->v2, STATE persisted)"; exit 0
    fi
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died during upgrade"; tail -200 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "node never came up as v2:"; ctl version || true; ctl get UpgradeStatus --namespace runtime || true
tail -200 "$SERIAL"; exit 1
