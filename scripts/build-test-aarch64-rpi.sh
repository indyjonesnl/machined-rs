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

# Assert the FAT carries the Pi boot files. Walk the FAT32 root directory with
# python (no mount/losetup needed) and collect the long file names: every Pi
# boot file has an LFN entry storing the literal lowercase name. Parsing the
# directory (rather than grepping raw bytes) avoids matching the file *contents*
# of the staged initramfs/blobs, so the check can't pass vacuously.
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
# Walk the root-dir cluster chain.
entries, clus, guard = b"", root_clus, 0
while 2 <= clus < 0x0FFFFFF8 and guard < 100000:
    entries += clus_bytes(clus); clus = fat_entry(clus); guard += 1
def dec_lfn(e):
    return (e[1:11] + e[14:26] + e[28:32]).decode("utf-16-le", "replace").split("\x00")[0]
names, lfn = set(), ""
for i in range(0, len(entries), 32):
    e = entries[i:i + 32]
    if len(e) < 32 or e[0] == 0x00:
        break
    if e[0] == 0xE5:
        lfn = ""; continue
    if e[11] == 0x0F:            # LFN component
        lfn = dec_lfn(e) + lfn; continue
    short = e[0:8].rstrip().decode("latin1")
    ext = e[8:11].rstrip().decode("latin1")
    short_name = short + ("." + ext if ext else "")
    names.add(lfn or short_name); lfn = ""
want = ["config.txt", "cmdline.txt", "bootcode.bin", "start.elf", "fixup.dat",
        "vmlinuz", "initramfs.img", "bcm2837-rpi-3-a-plus.dtb"]
missing = [w for w in want if w not in names]
if missing:
    print("MISSING from FAT root dir:", missing)
    print("present:", sorted(names)); sys.exit(1)
print("FAT carries the Pi boot files: " + ", ".join(want))
PY

echo "aarch64-rpi image built + Pi boot files present: BUILD CHECK PASSED"
