#!/usr/bin/env bash
# Boot an aarch64 MBR-partitioned image under qemu-system-aarch64 -M virt (TCG)
# and assert machined mounts the FAT /boot off the MBR disk.
#
# WHY THIS EXISTS:
#   The Raspberry Pi boots from an MBR SD card (Pi firmware reads MBR, not GPT),
#   so machined has an MBR code path: its block scan tries GPT, fails, and falls
#   back to enumerating partitions straight from sysfs (block/src/sysfs.rs), then
#   mounts the first vfat partition at /boot (machined/src/imageboot.rs).
#
#   That path CANNOT be exercised by boot-test-aarch64-rpi.sh: qemu's raspi3 SD
#   model (sdhost) enumerates the card but never exposes its MBR partition table
#   to the Alpine linux-rpi kernel, so mmcblk0pN is never created (a documented
#   qemu limitation — see boot-test-aarch64-rpi.sh). So we boot the SAME MBR
#   layout on -M virt instead, where qemu's virtio-blk DOES expose vda1 to the
#   kernel. The image uses the qemu-virt kernel (arch "aarch64-mbr": virt kernel,
#   MBR scheme, no Pi firmware) so it boots cleanly on -M virt.
#
# This is pure TCG (cross-arch, no KVM), but the assertion fires the instant
# machined mounts /boot (a few seconds of guest time), so it is quick.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET_DIR=${CARGO_TARGET_DIR:-target}
WORK=target/boot-test-mbr
IMG=$WORK/machined-aarch64-mbr.img
SERIAL=$WORK/serial.log
MACHINED=$TARGET_DIR/aarch64-unknown-linux-musl/release/machined
IMAGER=$TARGET_DIR/release/machined-imager
TIMEOUT=${BOOT_TEST_TIMEOUT:-600}
MARKER=${BOOT_TEST_MARKER:-'mounted boot partition .* at /boot'}

command -v qemu-system-aarch64 >/dev/null || { echo "FATAL: qemu-system-aarch64 not installed"; exit 2; }
[ -x "$MACHINED" ] || { echo "FATAL: $MACHINED missing — run make dist-aarch64"; exit 2; }
[ -x "$IMAGER" ]   || { echo "FATAL: $IMAGER missing — cargo build --release -p machined-imager"; exit 2; }

rm -rf "$WORK"; mkdir -p "$WORK"
"$IMAGER" gen-pki --out "$WORK/pki"
# arch aarch64-mbr = the qemu-virt kernel with the Pi's MBR partition table, so
# the image boots on -M virt but exercises the MBR/vfat /boot path the Pi uses.
"$IMAGER" build --arch aarch64-mbr --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

echo "image:     $IMG ($(du -h "$IMG" | cut -f1))"
file "$IMG"
echo "kernel:    $WORK/boot/vmlinuz ($(du -h "$WORK/boot/vmlinuz" | cut -f1))"
echo "initramfs: $WORK/boot/initramfs.img ($(du -h "$WORK/boot/initramfs.img" | cut -f1))"

# -M virt + EFI firmware: the Alpine aarch64 vmlinuz-virt is an EFI-stub kernel
# (edk2 loads kernel+initrd from fw_cfg), same as boot-test-aarch64.sh. The disk
# is attached as virtio-blk, whose partition table the kernel reads normally —
# so vda1 (the MBR FAT partition) appears and machined can mount it.
qemu-system-aarch64 -machine virt -cpu cortex-a53 -smp 2 -m 512 \
  -bios /usr/share/qemu-efi-aarch64/QEMU_EFI.fd \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -append "console=ttyAMA0" \
  -drive file="$IMG",if=virtio,format=raw \
  -display none -serial "file:$SERIAL" &
QEMU=$!
trap 'kill $QEMU 2>/dev/null || true; wait $QEMU 2>/dev/null || true' EXIT

echo "waiting for serial marker /$MARKER/ (max ${TIMEOUT}s)..."
deadline=$((SECONDS + TIMEOUT))
while [ "$SECONDS" -lt "$deadline" ]; do
  if grep -Eq "$MARKER" "$SERIAL" 2>/dev/null; then
    echo "MARKER HIT:"; grep -E "$MARKER" "$SERIAL" | head -3
    echo "MBR /boot MOUNT TEST PASSED"; exit 0
  fi
  if ! kill -0 "$QEMU" 2>/dev/null; then echo "QEMU exited early"; tail -80 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "TIMEOUT — marker never appeared"; tail -120 "$SERIAL"; exit 1
