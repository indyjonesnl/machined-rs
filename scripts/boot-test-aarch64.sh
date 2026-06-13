#!/usr/bin/env bash
# Boot the aarch64 image in QEMU (qemu-system-aarch64 -M virt, TCG) and assert the
# node comes up end-to-end:
#   - the mTLS management API answers (machinectl version)
#   - STATE + EPHEMERAL volumes are provisioned (CompleteLayout ran)
#
# This is the aarch64 / qemu-virt (TCG) variant of boot-test-x86_64.sh. Cross-arch
# means no KVM (pure TCG emulation), so it is slow — the timeouts are generous.
#
# Binaries are resolved via $CARGO_TARGET_DIR (this host redirects cargo output
# out of ./target), but the work dir stays under the repo's ./target so image
# artifacts, the imager cache, and CI paths remain stable and predictable.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET_DIR=${CARGO_TARGET_DIR:-target}
WORK=target/boot-test
IMG=$WORK/machined-aarch64.img
SERIAL=$WORK/serial.log
MACHINED=$TARGET_DIR/aarch64-unknown-linux-musl/release/machined
IMAGER=$TARGET_DIR/release/machined-imager
CTL=$TARGET_DIR/release/machinectl
TIMEOUT=${BOOT_TEST_TIMEOUT:-600}
# Port 50000 may be busy on a dev host; override with BOOT_TEST_PORT.
PORT=${BOOT_TEST_PORT:-50000}

command -v qemu-system-aarch64 >/dev/null || { echo "FATAL: qemu-system-aarch64 not installed"; exit 2; }
[ -x "$MACHINED" ] || { echo "FATAL: $MACHINED missing — run make dist-aarch64"; exit 2; }
[ -x "$IMAGER" ]   || { echo "FATAL: $IMAGER missing — run cargo build --release -p machined-imager"; exit 2; }
[ -x "$CTL" ]      || { echo "FATAL: $CTL missing — run cargo build --release -p machinectl"; exit 2; }

rm -rf "$WORK"; mkdir -p "$WORK"

# Pre-baked node PKI: CA + server identity + machinectl client bundle under pki/machinectl/.
"$IMAGER" gen-pki --out "$WORK/pki"
# --cache is pinned (the imager default is repo-relative target/imager-cache anyway,
# but pin it so the cached Alpine artifacts are reused deterministically).
"$IMAGER" build --arch aarch64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

echo "image:     $IMG ($(du -h "$IMG" | cut -f1))"
echo "vmlinuz:   $WORK/boot/vmlinuz ($(du -h "$WORK/boot/vmlinuz" | cut -f1))"
echo "initramfs: $WORK/boot/initramfs.img ($(du -h "$WORK/boot/initramfs.img" | cut -f1))"

# cortex-a53 = the Pi 3A+ core; far faster under TCG than -cpu max (which emulates
# SVE etc.). -bios QEMU_EFI.fd: the Alpine aarch64 vmlinuz is an EFI-stub kernel
# that needs UEFI firmware to boot (qemu's bare -kernel on -M virt can't, unlike
# x86's bzImage). edk2 loads kernel+initrd from fw_cfg via the EFI stub.
qemu-system-aarch64 -machine virt -cpu cortex-a53 -smp 2 -m 512 \
  -bios /usr/share/qemu-efi-aarch64/QEMU_EFI.fd \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -append "console=ttyAMA0" \
  -drive file="$IMG",if=virtio,format=raw \
  -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${PORT}-:50000" \
  -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$SERIAL" &
QEMU=$!
trap 'kill $QEMU 2>/dev/null || true; wait $QEMU 2>/dev/null || true' EXIT

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
vol_deadline=$((SECONDS + 300))
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
rt_deadline=$((SECONDS + 300))
while [ $SECONDS -lt $rt_deadline ]; do
  RT=$(ctl get RuntimeStatus --namespace runtime 2>/dev/null || true)
  if echo "$RT" | grep -Eq 'ready=true'; then
    echo "$RT"; echo "BOOT TEST PASSED"; exit 0
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -80 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "runtime never became ready:"; ctl get RuntimeStatus --namespace runtime || true
tail -120 "$SERIAL"; exit 1
