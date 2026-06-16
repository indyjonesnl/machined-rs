#!/usr/bin/env bash
# Build the aarch64-rpi (Pi 3A+) image and assert the FAT carries the Pi boot
# files. NO boot — qemu can't emulate Pi firmware; the node is verified on real
# hardware. This proves the image builds + stages correctly.
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET_DIR=${CARGO_TARGET_DIR:-target}
WORK=target/boot-test
IMG=$WORK/machined-aarch64-rpi.img
MACHINED=$TARGET_DIR/aarch64-unknown-linux-musl/release/machined
IMAGER=$TARGET_DIR/release/machined-imager
rm -rf "$WORK"; mkdir -p "$WORK"
[ -x "$MACHINED" ] || { echo "FATAL: $MACHINED missing — run make dist-aarch64"; exit 2; }

"$IMAGER" gen-pki --out "$WORK/pki"
"$IMAGER" build --arch aarch64-rpi --machined "$MACHINED" \
  --config examples/node-pi.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --cache target/imager-cache

echo "image:     $IMG ($(du -h "$IMG" | cut -f1))"

# Assert MBR: signature 0x55AA, partition 1 bootable (0x80) FAT32-LBA (0x0C) at LBA 2048.
python3 - "$IMG" <<'PY'
import sys, struct
raw = open(sys.argv[1], "rb").read(512)
assert raw[510:512] == b"\x55\xAA", "no MBR signature"
assert raw[446] == 0x80, "partition 1 not bootable"
assert raw[446+4] == 0x0C, "partition 1 not FAT32-LBA"
lba = struct.unpack("<I", raw[446+8:446+12])[0]
assert lba == 2048, f"FAT not at LBA 2048: {lba}"
assert raw[512:520] != b"EFI PART", "unexpected GPT header"
print(f"MBR OK: bootable FAT32 primary at LBA {lba}")
PY

# Assert the FAT carries the A/B os_prefix slot layout. Walk the FAT32 directory
# tree recursively with python (no mount/losetup needed): config.txt + firmware
# blobs live at the root, the full slot (kernel/initramfs/dtb/cmdline/overlays)
# in /A, and the scaffold-only slot (dtb/cmdline/overlays, NO kernel yet) in /B.
# Parsing the directory entries (rather than grepping raw bytes) avoids matching
# the staged initramfs/blob *contents*, so the check can't pass vacuously.
python3 - "$IMG" <<'PY'
import sys, struct
data = open(sys.argv[1], "rb").read()
base = 2048 * 512
bpb = data[base:base + 512]
bps = struct.unpack("<H", bpb[11:13])[0]
spc = bpb[13]
rsvd = struct.unpack("<H", bpb[14:16])[0]
nfats = bpb[16]
fatsz = struct.unpack("<I", bpb[36:40])[0]
root_clus = struct.unpack("<I", bpb[44:48])[0]
first_data = rsvd + nfats * fatsz
fat_off = base + rsvd * bps
def fat_entry(c):
    return struct.unpack("<I", data[fat_off + c * 4:fat_off + c * 4 + 4])[0] & 0x0FFFFFFF
def clus_bytes(c):
    sec = first_data + (c - 2) * spc
    off = base + sec * bps
    return data[off:off + spc * bps]
def chain_bytes(first):
    out, c, guard = b"", first, 0
    while 2 <= c < 0x0FFFFFF8 and guard < 100000:
        out += clus_bytes(c); c = fat_entry(c); guard += 1
    return out
def dec_lfn(e):
    return (e[1:11] + e[14:26] + e[28:32]).decode("utf-16-le", "replace").split("\x00")[0]
def read_dir(first):
    """name -> ('dir'|'file', first_cluster) for the directory at cluster `first`."""
    entries, lfn, out = chain_bytes(first), "", {}
    for i in range(0, len(entries), 32):
        e = entries[i:i + 32]
        if len(e) < 32 or e[0] == 0x00:
            break
        if e[0] == 0xE5:
            lfn = ""; continue
        if e[11] == 0x0F:
            lfn = dec_lfn(e) + lfn; continue
        short = e[0:8].rstrip().decode("latin1")
        ext = e[8:11].rstrip().decode("latin1")
        name = lfn or (short + ("." + ext if ext else "")); lfn = ""
        if name in (".", ".."):
            continue
        fc = (struct.unpack("<H", e[20:22])[0] << 16) | struct.unpack("<H", e[26:28])[0]
        out[name] = ("dir" if (e[11] & 0x10) else "file", fc)
    return out
def need(d, name, kind, where):
    assert name in d, f"missing {name} in {where} (have {sorted(d)})"
    assert d[name][0] == kind, f"{where}/{name} is {d[name][0]}, want {kind}"
    return d[name][1]
def text(first):  # whole clusters (trailing padding is harmless for substring checks)
    return chain_bytes(first).decode("latin1")

root = read_dir(root_clus)
# Root: config.txt + firmware blobs; the kernel/initramfs/dtb/cmdline are now in /A.
for f in ["config.txt", "bootcode.bin", "start.elf", "fixup.dat"]:
    need(root, f, "file", "root")
for f in ["vmlinuz", "initramfs.img", "cmdline.txt", "bcm2837-rpi-3-a-plus.dtb"]:
    assert f not in root, f"{f} must NOT be at FAT root (moved into /A)"
assert "os_prefix=A/" in text(root["config.txt"][1]), "config.txt missing os_prefix=A/"
a = read_dir(need(root, "A", "dir", "root"))
b = read_dir(need(root, "B", "dir", "root"))
# Slot A: full (kernel staged here at build).
for f in ["vmlinuz", "initramfs.img", "bcm2837-rpi-3-a-plus.dtb", "cmdline.txt"]:
    need(a, f, "file", "A")
assert "disable-bt.dtbo" in read_dir(need(a, "overlays", "dir", "A")), "A/overlays/disable-bt.dtbo missing"
assert "machined.slot=a" in text(a["cmdline.txt"][1]), "A/cmdline.txt missing machined.slot=a"
# Slot B: scaffolding only — NO kernel until the first upgrade.
for f in ["bcm2837-rpi-3-a-plus.dtb", "cmdline.txt"]:
    need(b, f, "file", "B")
assert "vmlinuz" not in b, "B/vmlinuz must be absent until the first upgrade"
assert "disable-bt.dtbo" in read_dir(need(b, "overlays", "dir", "B")), "B/overlays/disable-bt.dtbo missing"
assert "machined.slot=b" in text(b["cmdline.txt"][1]), "B/cmdline.txt missing machined.slot=b"
print("A/B os_prefix slot layout OK: root=config.txt+blobs, /A=full slot, /B=scaffold")
PY

echo "aarch64-rpi A/B image built + os_prefix slot layout verified: BUILD CHECK PASSED"
