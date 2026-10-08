#!/usr/bin/env bash
# Compile JavaScript into a unit image a graph can carry as config.
#
#   packaging/embedded/image.sh <out.img> <source.js>...
#
# The sources are joined in order -- surface.js first, then the program -- and
# compiled on this host by `phasor_compile`, which needs far more module state
# than a microcontroller has. The image is written as it is, for the
# isolate's `image` parameter to carry in the graph's config:
#
#   modules:
#     - name: phasor_isolate
#       params: { image: "${file:target/embedded/program.img}" }
#
# The runtime can deliver a stream short when the machine is loaded, so the
# compile runs until two runs agree on the bytes.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
out=$1
shift
[ "$#" -gt 0 ] || { echo "usage: image.sh <out.img> <source.js>..." >&2; exit 2; }

RUNTIME="$ROOT/target/aarch64-unknown-linux-gnu/release/fluxor-linux"
GRAPH="$ROOT/target/linux/embedded_compile"
fluxor build "$ROOT/packaging/embedded/embedded_compile.yaml" > /dev/null 2>&1

work=$(mktemp -d "${TMPDIR:-/tmp}/phasor-image.XXXXXX")
trap 'rm -rf "$work"' EXIT
cat "$@" > "$work/source.js"

compile() {
    "$RUNTIME" --config "$GRAPH/config.bin" --modules "$GRAPH/modules.bin" \
        < "$work/source.js" > "$1" 2> "$work/log"
}
# A compile that fails says why, rather than ending the script on `set -e`.
compile "$work/a" || {
    echo "image.sh: the compiler refused the source:" >&2
    grep -avE "^\[[0-9]+ (INFO|DEBUG)" "$work/log" | tail -8 >&2
    exit 1
}
agreed=
for _ in 1 2 3 4; do
    compile "$work/b" || true
    if cmp -s "$work/a" "$work/b"; then agreed=1; break; fi
    # Keep the longer: a short stream is a prefix of a whole one.
    [ "$(stat -c %s "$work/b")" -gt "$(stat -c %s "$work/a")" ] && mv "$work/b" "$work/a"
done
[ -n "$agreed" ] || { echo "image.sh: no two compiles agreed" >&2; exit 1; }
[ -s "$work/a" ] || { echo "image.sh: the compiler produced no image" >&2; tail -5 "$work/log" >&2; exit 1; }

mkdir -p "$(dirname "$out")"
cp "$work/a" "$out"
echo "image.sh: $(stat -c %s "$work/a") bytes -> $out"
