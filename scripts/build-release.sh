#!/bin/bash
# SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Build one release archive for the machine this runs on.
#
# Usage: scripts/build-release.sh VERSION OUTDIR
#        scripts/build-release.sh v2026.9.30 dist
#
# CI runs exactly this on one native runner per architecture (amd64 and
# arm64) and joins the results with SHA256SUMS, so a release can be checked
# before tagging on any Linux machine with cargo. The archive name carries no
# version, so .../releases/latest/download/<name> always means the newest.
set -euo pipefail

VERSION="${1:?usage: $0 VERSION OUTDIR}"
OUT="${2:?usage: $0 VERSION OUTDIR}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

case "$(uname -m)" in
  x86_64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# The tag names the version the binary reports, so the two cannot disagree.
want="v$(cd "$ROOT" && cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')"
[ "$VERSION" = "$want" ] || { echo "tag $VERSION does not match Cargo.toml ($want)" >&2; exit 1; }

name="inbuxa-migrate-linux-$arch"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

echo "==> building $name ($VERSION)"
(cd "$ROOT" && cargo build --release --locked)
mkdir -p "$STAGE/$name"
cp "$ROOT/target/release/inbuxa-migrate" "$ROOT/README.md" "$STAGE/$name/"
cp -r "$ROOT/LICENSES" "$STAGE/$name/"
# Fixed owner, order and time, so the archive's layout and metadata do not
# change from one build of a tag to the next.
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@${SOURCE_DATE_EPOCH:-0}" \
  -C "$STAGE/$name" -czf "$OUT/$name.tar.gz" inbuxa-migrate LICENSES README.md
echo "==> $OUT/$name.tar.gz"
