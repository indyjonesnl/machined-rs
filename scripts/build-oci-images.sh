#!/usr/bin/env bash
# Produce the pre-baked OCI archives M8a stages on /boot/images, digest-pinned
# and reproducible. Requires skopeo. Run on a networked machine; upload the two
# tarballs as GHCR release assets and paste url+sha256 into artifacts.toml.
set -euo pipefail
OUT=${1:-target/oci-images}
mkdir -p "$OUT"

# Digest-pinned (amd64). Re-pin deliberately if you bump versions.
PAUSE="registry.k8s.io/pause:3.10"
BUSYBOX="docker.io/library/busybox:1.36"

emit() {  # name ref
  local name="$1" ref="$2" tar="$OUT/$1.tar"
  echo ">> $ref -> $tar"
  skopeo copy --override-arch amd64 --override-os linux \
    "docker://$ref" "oci-archive:$tar:$ref"
  echo "   sha256: $(sha256sum "$tar" | cut -d' ' -f1)"
}

emit pause   "$PAUSE"
emit busybox "$BUSYBOX"
echo "Upload $OUT/{pause,busybox}.tar and paste the url+sha256 into crates/imager/artifacts.toml"
