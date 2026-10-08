#!/usr/bin/env bash
# Build every program image a graph names.
#
#   packaging/embedded/images-for.sh <graph.yaml>
#
# A graph names an image as `target/embedded/<name>.img`; it is built from
# surface.js and `examples/embedded/<name>.js`, the program, by image.sh. A
# rig recipe runs this before it packs, so the image a board flashes is the
# one compiled from the source beside it.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
graph=$1
for image in $(grep -oE 'target/embedded/[A-Za-z0-9_]+\.img' "$graph" | sort -u); do
    name=$(basename "$image" .img)
    "$ROOT/packaging/embedded/image.sh" "$ROOT/$image" \
        "$ROOT/packaging/embedded/surface.js" "$ROOT/examples/embedded/$name.js"
done
