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

## Known hardware-gated items (report if they differ)
- **MBR vs GPT:** the image is MBR (Pi 3 firmware reads MBR). If a future Pi
  needs GPT, that's a separate change.
- **DTB:** `config.txt` sets `device_tree=bcm2837-rpi-3-a-plus.dtb` explicitly.
  If your board's firmware prefers auto-select, dropping that line also works.
- **Console:** `console=serial0,115200` maps to the PL011 with `disable-bt`. If
  the serial is silent, try `console=ttyAMA0,115200` or `console=ttyS0,115200`.
- **No persistence:** PKI is seeded from /boot each boot; containerd's root is on
  the initramfs (ephemeral). Persistent volumes on Pi = a future milestone.
