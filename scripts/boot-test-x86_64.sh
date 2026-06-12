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
TIMEOUT=${BOOT_TEST_TIMEOUT:-150}
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
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

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
trap 'kill $QEMU 2>/dev/null || true; wait $QEMU 2>/dev/null || true' EXIT

ctl() { "$CTL" --bundle "$WORK/pki/machinectl" --endpoint "https://127.0.0.1:${PORT}" "$@"; }

echo "waiting for API (max ${TIMEOUT}s)..."
for i in $(seq "$TIMEOUT"); do
  if ctl version >/dev/null 2>&1; then break; fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -50 "$SERIAL"; exit 1; fi
  if [ "$i" = "$TIMEOUT" ]; then echo "TIMEOUT waiting for API"; tail -80 "$SERIAL"; exit 1; fi
  sleep 1
done
echo "API up: $(ctl version)"

# machinectl `get` prints one row per resource: "<id>\t<k=v k=v ...>".
# A provisioned VolumeStatus row looks like:
#   STATE     name=STATE device=/dev/vda2 fs=ext4 label=STATE phase=Provisioned
# Assert both managed volumes are present AND Provisioned (CompleteLayout ran).
echo "checking provisioned volumes (namespace block)..."
for i in $(seq 60); do
  VOLS=$(ctl get VolumeStatus --namespace block 2>/dev/null || true)
  if echo "$VOLS" | grep -Eq 'name=STATE .*phase=Provisioned' \
     && echo "$VOLS" | grep -Eq 'name=EPHEMERAL .*phase=Provisioned'; then
    echo "$VOLS"; echo "BOOT TEST PASSED"; exit 0
  fi
  sleep 2
done
echo "volumes never provisioned (STATE+EPHEMERAL phase=Provisioned):"
ctl get VolumeStatus --namespace block || true
tail -80 "$SERIAL"; exit 1
