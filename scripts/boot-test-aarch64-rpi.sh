#!/usr/bin/env bash
# Boot the aarch64-rpi (Pi 3A+) image under qemu-system-aarch64 -M raspi3ap and
# assert the node reaches userland by watching the serial console.
#
# This is the BOOT counterpart to scripts/build-test-aarch64-rpi.sh (which only
# builds the image + reads the FAT back, no boot). raspi3ap models the real
# BCM2837 SoC: it attaches the image as the SD card (-> /dev/mmcblk0), so this
# exercises what the build-only check cannot — the SD/MMC controller, the MBR
# partition-table read, and the vfat /boot mount on a Pi-shaped machine.
#
# WHAT THIS PROVES — AND WHAT IT DOES NOT:
#   - DOES: the cross-built aarch64 machined + Pi initramfs boot on the BCM2837
#     SoC model, the kernel enumerates the SD controller as mmcblk0, machined
#     parses the MBR and mounts the FAT /boot.
#   - DOES NOT: model the VideoCore firmware chain (bootcode.bin -> start.elf ->
#     config.txt). QEMU's -kernel/-initrd boot stands in for the firmware-loaded
#     pair and bypasses that handoff entirely. A green run here is necessary but
#     NOT sufficient: the firmware handoff is only proven on real hardware
#     (see docs/raspberry-pi-3a-plus.md).
#
# Cross-arch emulation is pure TCG (KVM can't accelerate a foreign arch) and the
# Pi SoC model is slower than -M virt, so the timeout is generous.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET_DIR=${CARGO_TARGET_DIR:-target}
WORK=target/boot-test-rpi
IMG=$WORK/machined-aarch64-rpi.img
SERIAL=$WORK/serial.log
MACHINED=$TARGET_DIR/aarch64-unknown-linux-musl/release/machined
IMAGER=$TARGET_DIR/release/machined-imager
TIMEOUT=${BOOT_TEST_TIMEOUT:-900}
# Default marker proves the meaningful coverage: the SD read + MBR parse + vfat
# mount all succeeded (machined logs this from pid1, imageboot.rs). Override
# with BOOT_TEST_MARKER for a stronger/different assertion.
MARKER=${BOOT_TEST_MARKER:-'mounted boot partition .* at /boot'}

command -v qemu-system-aarch64 >/dev/null || { echo "FATAL: qemu-system-aarch64 not installed"; exit 2; }
qemu-system-aarch64 -M help 2>/dev/null | grep -q raspi3ap || {
  echo "FATAL: this QEMU has no raspi3ap machine (need qemu >= 7.1)"; exit 2; }
[ -x "$MACHINED" ] || { echo "FATAL: $MACHINED missing — run make dist-aarch64"; exit 2; }
[ -x "$IMAGER" ]   || { echo "FATAL: $IMAGER missing — cargo build --release -p machined-imager"; exit 2; }

rm -rf "$WORK"; mkdir -p "$WORK"
"$IMAGER" gen-pki --out "$WORK/pki"
# --emit-boot drops vmlinuz + initramfs.img beside the image. On real hardware
# the Pi firmware loads these from the FAT per config.txt; QEMU's -kernel boot
# bypasses the firmware, so we hand the pair to QEMU directly.
"$IMAGER" build --arch aarch64-rpi --machined "$MACHINED" \
  --config examples/node-pi.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

echo "image:     $IMG ($(du -h "$IMG" | cut -f1))"
echo "kernel:    $WORK/boot/vmlinuz ($(du -h "$WORK/boot/vmlinuz" | cut -f1))"
echo "initramfs: $WORK/boot/initramfs.img ($(du -h "$WORK/boot/initramfs.img" | cut -f1))"

# raspi3ap = the Pi 3A+ SoC model: fixed 512 MiB RAM, the SD host controller
# (the image attaches as the SD card -> /dev/mmcblk0), the PL011 UART (ttyAMA0).
#   no -bios:  the Pi has no UEFI; QEMU boots the raw -kernel directly.
#   no -dtb:   QEMU generates a device tree matching ITS machine model; passing
#              the real Pi DTB would describe peripherals QEMU doesn't emulate.
qemu-system-aarch64 -M raspi3ap -m 512 \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -append "console=ttyAMA0,115200" \
  -drive file="$IMG",format=raw,if=sd \
  -display none -serial "file:$SERIAL" &
QEMU=$!
trap 'kill $QEMU 2>/dev/null || true; wait $QEMU 2>/dev/null || true' EXIT

echo "waiting for serial marker /$MARKER/ (max ${TIMEOUT}s)..."
deadline=$((SECONDS + TIMEOUT))
while [ "$SECONDS" -lt "$deadline" ]; do
  if grep -Eq "$MARKER" "$SERIAL" 2>/dev/null; then
    echo "MARKER HIT:"; grep -E "$MARKER" "$SERIAL" | head -3
    echo "RPI raspi3ap BOOT TEST PASSED"; exit 0
  fi
  if ! kill -0 "$QEMU" 2>/dev/null; then echo "QEMU exited early"; tail -80 "$SERIAL"; exit 1; fi
  sleep 3
done
echo "TIMEOUT — marker never appeared"; tail -120 "$SERIAL"; exit 1
