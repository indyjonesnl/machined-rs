#!/usr/bin/env bash
# Build the custom machined kernel: Alpine linux-virt 6.12.93's .config with
# CONFIG_KEXEC_FILE=y enabled, so machined's in-process kexec_file_load(2) OS
# upgrade works (stock Alpine ships KEXEC_FILE off; it also disables the old
# kexec_load syscall, so the file syscall is the only viable path — the same one
# Talos uses).
#
# Produces a tar.gz in the Alpine-apk layout the imager consumes (boot/vmlinuz-virt
# + lib/modules/<kver>/ with uncompressed .ko + a depmod-generated modules.dep) and
# prints its sha256 to pin in crates/imager/artifacts.toml (x86_64 linux-virt).
#
# Host deps (Debian/Ubuntu/Zorin): gcc make bc flex bison libelf-dev libssl-dev.
# (pahole/dwarves NOT needed — this build disables BTF.)
#
# Usage: scripts/build-kexec-kernel.sh [workdir]
#   workdir defaults to target/kbuild. The Alpine .config is read from the cached
#   linux-virt apk under target/imager-cache (run a normal `make boot-test`/imager
#   build first, or drop the apk there).
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)

KVER_BASE=6.12.93
LOCALVERSION="-machined-virt"
KVER="${KVER_BASE}${LOCALVERSION}"
WORK=${1:-target/kbuild}
SRC="$WORK/linux-${KVER_BASE}"
# sha256 of the cached Alpine x86_64 linux-virt-6.12.93-r0.apk (source of the base .config).
ALPINE_KERNEL_SHA=46851b49c0c8f7d689315556caff54df457238fd9c575b4e16837031a6653c27

mkdir -p "$WORK"

# 1. Kernel source (vanilla; Alpine linux-virt is vanilla 6.12.93 + minimal patches).
if [ ! -d "$SRC" ]; then
  echo ">> fetching linux-${KVER_BASE} source"
  curl -fsSL -o "$WORK/linux-${KVER_BASE}.tar.xz" \
    "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KVER_BASE}.tar.xz"
  tar -xf "$WORK/linux-${KVER_BASE}.tar.xz" -C "$WORK"
fi
cd "$SRC"

# 2. Base config = Alpine linux-virt's, adapted to vanilla + the deltas we need.
echo ">> applying Alpine .config + kexec/build deltas"
tmp=$(mktemp -d)
tar -xzf "$REPO/target/imager-cache/$ALPINE_KERNEL_SHA" -C "$tmp"
cp "$(find "$tmp" -name 'config-*' | head -1)" .config
rm -rf "$tmp"
make olddefconfig >/dev/null
./scripts/config \
  --enable  KEXEC --enable KEXEC_FILE --disable KEXEC_SIG \
  --disable DEBUG_INFO_BTF --disable DEBUG_INFO_BTF_MODULES \
  --disable DEBUG_INFO --enable DEBUG_INFO_NONE \
  --disable MODULE_SIG --disable MODULE_SIG_ALL --disable MODULE_SIG_FORCE \
  --set-str MODULE_SIG_KEY "" --set-str SYSTEM_TRUSTED_KEYS "" --set-str SYSTEM_REVOCATION_KEYS "" \
  --module  NLS_ISO8859_1 \
  --set-str LOCALVERSION "$LOCALVERSION" --disable LOCALVERSION_AUTO
make olddefconfig >/dev/null
grep -qE '^CONFIG_KEXEC_FILE=y' .config || { echo "FATAL: KEXEC_FILE not enabled"; exit 1; }

# 3. Build.
echo ">> building bzImage + modules (-j$(nproc))"
make -j"$(nproc)" bzImage modules

# 4. Stage modules; the imager wants .ko (it decompresses .ko.gz on extract, and a
#    plain .ko passes through). The host depmod often lacks zlib (can't read .ko.gz),
#    so install, gunzip, strip, and depmod against uncompressed .ko.
STAGE="$WORK/stage"
rm -rf "$STAGE"
make -s -j"$(nproc)" INSTALL_MOD_PATH="$STAGE" modules_install
find "$STAGE/lib/modules/$KVER" -name '*.ko.gz' -exec gunzip -f {} +
find "$STAGE/lib/modules/$KVER" -name '*.ko'    -exec strip --strip-debug {} +
depmod -b "$STAGE" -F System.map "$KVER"
test -s "$STAGE/lib/modules/$KVER/modules.dep" || { echo "FATAL: empty modules.dep"; exit 1; }

# 5. Package in the Alpine-apk layout the imager extracts.
PKG="$WORK/pkg"
rm -rf "$PKG"; mkdir -p "$PKG/boot" "$PKG/lib/modules"
cp arch/x86/boot/bzImage "$PKG/boot/vmlinuz-virt"
cp -a "$STAGE/lib/modules/$KVER" "$PKG/lib/modules/"
printf 'pkgname = linux-virt-machined\npkgver = %s\n' "$KVER" > "$PKG/.PKGINFO"
OUT="$WORK/custom-kernel-virt-${KVER_BASE}-machined.tar.gz"
( cd "$PKG" && tar czf "$REPO/$OUT" .PKGINFO boot lib )

SHA=$(sha256sum "$REPO/$OUT" | cut -d' ' -f1)
echo
echo "built: $OUT ($(du -h "$REPO/$OUT" | cut -f1))"
echo "sha256: $SHA"
echo
echo "Next: host this tar.gz on a GHCR release and set its url + the sha256 above"
echo "on the x86_64 linux-virt entry in crates/imager/artifacts.toml. For a local"
echo "boot-test, seed the imager cache so no fetch is needed:"
echo "  cp $OUT target/imager-cache/$SHA"
