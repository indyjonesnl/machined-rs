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
#   - DOES: the cross-built aarch64 machined + Pi initramfs boot to userland on
#     the real BCM2837 SoC model — the kernel comes up, machined runs as pid1
#     and reaches "node up". This exercises the Pi kernel + the machined boot
#     sequence on a Pi-shaped machine.
#   - DOES NOT: mount the SD card's MBR /boot partition. QEMU's raspi3 SD model
#     (sdhost) enumerates the card but does NOT expose its MBR partition table to
#     a modern Alpine linux-rpi kernel — mmcblk0pN is never created, so machined
#     can't mount /boot here. This is a documented QEMU limitation, not a bug in
#     machined: the qemu raspi3 SD/sdhost path is known-unreliable for partition
#     access (the upstream guides that DO see mmcblk0pN use a different kernel —
#     Debian-generic / Pi-Foundation kernel8.img — or avoid the raspi SD entirely
#     via -M versatilepb + IDE). The MBR-read + vfat-mount CODE PATH is instead
#     covered under -M virt (where qemu exposes partitions) by
#     scripts/boot-test-aarch64-mbr.sh; the SD/firmware handoff itself is proven
#     on real hardware (see docs/raspberry-pi-3a-plus.md).
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
# Default marker proves the meaningful coverage QEMU raspi3 CAN deliver: the Pi
# kernel booted to userland on the BCM2837 model and machined came up as pid1.
# (The SD /boot mount is NOT asserted here — qemu's raspi3 SD model never exposes
# the MBR partition table to this kernel; that code path is covered on -M virt by
# boot-test-aarch64-mbr.sh. See the header.) Override with BOOT_TEST_MARKER.
MARKER=${BOOT_TEST_MARKER:-'boot complete; node up'}

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
#   -dtb:      REQUIRED. QEMU's auto-generated raspi3ap device tree is
#              incompatible with the Alpine linux-rpi kernel — it hangs in head.S
#              before emitting any serial (silent zero-output failure). The real
#              board DTB (the same one the imager stages into the FAT for the
#              firmware boot, emitted beside vmlinuz by --emit-boot) boots cleanly;
#              the kernel even IDs the model. raspi3ap wires the PL011 to serial0,
#              so console=ttyAMA0 lands on -serial; earlycon forces output the
#              instant the kernel runs, so any future regression surfaces as real
#              serial rather than silence.
# Kernel cmdline, each token load-bearing under qemu raspi3ap:
#   earlycon=pl011,…           force serial from the first instruction (so a
#                              regression shows real output, never silence).
#   console=ttyAMA0,115200     raspi3ap wires the PL011 to serial0.
#   initcall_blacklist=bcm2835_pm_driver_init
#                              bcm2835_power_probe touches a PM register QEMU
#                              doesn't emulate → synchronous external abort that
#                              kills init. Skipping that one initcall avoids it;
#                              the power domain is unused for this boot.
#   clk_ignore_unused pd_ignore_unused
#                              stop the kernel disabling "unused" clocks/power
#                              domains mid-SD-init (which re-enumerates the card).
qemu-system-aarch64 -M raspi3ap -m 512 \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -dtb "$WORK/boot/bcm2837-rpi-3-a-plus.dtb" \
  -append "earlycon=pl011,0x3f201000 console=ttyAMA0,115200 initcall_blacklist=bcm2835_pm_driver_init clk_ignore_unused pd_ignore_unused" \
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
