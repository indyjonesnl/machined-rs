# Booting machined on a Raspberry Pi 3A+

machined-rs builds a Pi 3A+ SD-card image with `machined-imager build --arch
aarch64-rpi`. CI builds + validates the image, but cannot boot it (QEMU doesn't
emulate the Pi VideoCore firmware) — verify on real hardware over serial.

## Build + flash
1. `make dist-aarch64 && cargo build --release -p machined-imager`
2. `target/release/machined-imager gen-pki --out /tmp/pki`
3. `target/release/machined-imager build --arch aarch64-rpi \
     --machined target/aarch64-unknown-linux-musl/release/machined \
     --config examples/node-pi.yaml --pki-dir /tmp/pki \
     --out machined-pi.img`
4. Flash: `sudo dd if=machined-pi.img of=/dev/sdX bs=4M conv=fsync` (X = your SD reader; double-check the device!).

## Serial console
The Pi 3A+ has no Ethernet — verify over serial. Wire a 3.3 V USB-UART to the
GPIO header: GND -> pin 6, TX -> pin 8 (GPIO14), RX -> pin 10 (GPIO15).
`config.txt` sets `enable_uart=1` + `dtoverlay=disable-bt` (PL011 on the header).
Open at **115200 8N1** (`screen /dev/ttyUSB0 115200` or `picocom -b 115200 /dev/ttyUSB0`).

## What you should see
Power on; the GPU firmware loads `bootcode.bin -> start.elf`, then the kernel +
initramfs. On serial:
- the kernel boots (Linux 6.12.13-...-rpi)
- `machined starting (pid 1)`
- `booted from A/B slot a` (slot A on a fresh flash; an upgrade switches to B — see the A/B section below)
- `mounted boot partition /dev/mmcblk0p1 at /boot` (the vfat fallback)
- `seeded PKI from /boot/pki`
- `management API listening on 0.0.0.0:50000`
- `containerd successfully booted` (from /boot/bin)

No STATE/EPHEMERAL provisioning runs (the Pi image is MBR, not GPT — by design).

## Optional: machinectl over the network
Plug a USB-Ethernet adapter into the Pi's USB port; it appears as a wired NIC.
Add a `network.interfaces` entry to `node-pi.yaml` for it (static IP), rebuild,
reflash. Then from your workstation:
`machinectl --bundle /tmp/pki/machinectl --endpoint https://<pi-ip>:50000 version`.

## A/B upgrade (atomic, cold-reboot)
The Pi image is laid out for A/B updates. `config.txt` carries `os_prefix=A/`,
which the VideoCore firmware prepends when loading a self-contained slot dir
(`/A` or `/B`) — each holds `vmlinuz`, `initramfs.img`, the dtb, `cmdline.txt`,
and `overlays/disable-bt.dtbo`. The firmware blobs + `config.txt` itself stay at
the FAT root. machined upgrades by staging the inactive slot, then flipping the
`os_prefix` line to point at it.

The end-to-end upgrade can't be tested in QEMU (no firmware) — verify on real
hardware over serial. The Pi 3A+ has no Ethernet, so the node needs the
USB-Ethernet adapter from the section above to reach the bundle.

1. Build a v2 bundle on your workstation:
   ```
   target/release/machined-imager build --arch aarch64-rpi \
     --machined target/aarch64-unknown-linux-musl/release/machined \
     --config examples/node-pi.yaml --pki-dir /tmp/pki \
     --image-id v2 --out /tmp/machined-pi-v2.img --emit-boot /tmp/boot-v2
   tar -czf /tmp/bundle.tgz -C /tmp/boot-v2 vmlinuz initramfs.img
   sha256sum /tmp/bundle.tgz
   ```
   (`--out` is required by the imager but its `.img` isn't used here — the
   bundle is built from the `--emit-boot` dir.)
2. Serve it where the Pi can reach it: `cd /tmp && python3 -m http.server 8080`.
3. On the running v1 node (booted from slot A), trigger the upgrade:
   ```
   machinectl --bundle /tmp/pki/machinectl --endpoint https://<pi-ip>:50000 \
     upgrade http://<workstation-ip>:8080/bundle.tgz <sha256 of bundle.tgz from step 1>
   ```
   machined then downloads + verifies the bundle, stages it into the inactive
   slot `/B`, flips `config.txt` to `os_prefix=B/`, and reboots. Over serial
   you'll see it reboot, then the firmware boots `/B`.
4. Confirm the upgrade: over serial, machined logs `booted from A/B slot b`; and
   `machinectl … version` reports `image_id=v2`. The previous slot `/A` is
   untouched — it's the rollback target.

**Manual rollback:** to revert to v1, revert the `os_prefix` line. Mount the FAT boot
partition on your workstation (or edit on the Pi if writable) and change
`config.txt`'s `os_prefix=B/` back to `os_prefix=A/`, then reboot → the firmware
boots `/A` (v1). machined does not yet do automatic health-gated rollback —
that's a later milestone.

**Watch-out:** if the serial console goes dark after an upgrade, check that the
new slot has `overlays/disable-bt.dtbo`. `os_prefix` prepends to overlay paths,
and `disable-bt` is what puts the PL011 UART on the GPIO header. The imager
stages it into both slots; this is the first thing to check if a slot boots
silent.

## Known hardware-gated items (report if they differ)
- **MBR vs GPT:** the image is MBR (Pi 3 firmware reads MBR). If a future Pi
  needs GPT, that's a separate change.
- **DTB:** `config.txt` sets `device_tree=bcm2837-rpi-3-a-plus.dtb` explicitly.
  If your board's firmware prefers auto-select, dropping that line also works.
- **Console:** `console=serial0,115200` maps to the PL011 with `disable-bt`. If
  the serial is silent, try `console=ttyAMA0,115200` or `console=ttyS0,115200`.
- **No persistence:** PKI is seeded from /boot each boot; containerd's root is on
  the initramfs (ephemeral). Persistent volumes on Pi = a future milestone.
