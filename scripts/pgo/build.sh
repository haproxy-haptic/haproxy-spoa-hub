#!/usr/bin/env bash
# Run in the consuming repository, with its workload under ci/pgo/.
set -euo pipefail
target=${1:?usage: build.sh TARGET [cargo build options]}
shift
host=$(rustc -vV | sed -n 's/^host: //p')
tools=$(cd "$(dirname "$0")" && pwd)
profdata="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/raw"
runner=()
case "$target" in
    x86_64-unknown-linux-gnu) arch=amd64; sha=8654587cfc720a157cb419c588a9c192a9ea8a314763282405d7204b263821ae ;;
    aarch64-unknown-linux-gnu) arch=arm64; sha=6c76c5748e482f6790ebdf31057777fe88012507ee5edc24381c6d1e1164bfb7
        if [[ $target != "$host" ]]; then runner=(qemu-aarch64 -L /usr/aarch64-linux-gnu); fi ;;
    armv7-unknown-linux-gnueabihf) arch=armv7; sha=e321a152d3cd6836a14b2485cfafa8987bf5a96bbaa4a2913dc5a02171ee9bec
        if [[ $target != "$host" ]]; then runner=(qemu-arm -L /usr/arm-linux-gnueabihf); fi ;;
    *) echo "Unsupported release target: $target" >&2; exit 1 ;;
esac

cargo_build=(cargo auditable build --locked --release --target "$target" "$@")
# Explicit --target keeps host build scripts and proc macros out of the profile.
flags=()
if [[ -v CARGO_ENCODED_RUSTFLAGS ]]; then
    IFS=$'\x1f' read -ra flags <<< "$CARGO_ENCODED_RUSTFLAGS"
else
    read -ra flags <<< "${RUSTFLAGS:-}"
fi
export_flags() {
    local IFS=$'\x1f'
    local combined=("${flags[@]}" "$1")
    export CARGO_ENCODED_RUSTFLAGS="${combined[*]}"
}
export_flags "-Cprofile-generate=$work/raw"
CARGO_TARGET_DIR="$work/target" "${cargo_build[@]}"
out="$work/target/$target/release"
if [[ -x "$out/haproxy-spoa-hub" ]]; then
    hub="$out/haproxy-spoa-hub"
else
    hub="$work/hub"
    curl -fsSL --retry 3 -o "$hub" "https://github.com/haproxy-haptic/haproxy-spoa-hub/releases/download/v0.13.0/haproxy-spoa-hub-$arch-glibc2.36"
    echo "$sha  $hub" | sha256sum --check
    chmod +x "$hub"
fi

LLVM_PROFILE_FILE="$work/raw/%m-%p.profraw" python3 "$tools/train.py" \
    --hub "$hub" --plugins "$out" -- "${runner[@]}"
"$profdata" merge -o "$work/merged.profdata" "$work/raw"
"$profdata" show "$work/merged.profdata" | tee "$work/summary"
awk '/^Maximum function count:/ && $NF > 0 { seen = 1 } END { exit !seen }' "$work/summary"
rm -rf "$work/target" "$work/raw"
# Leave only the profile-use build in the packaging directory.
export_flags "-Cprofile-use=$work/merged.profdata"
"${cargo_build[@]}"
out="${CARGO_TARGET_DIR:-target}/$target/release"
if [[ -x "$out/haproxy-spoa-hub" ]]; then hub="$out/haproxy-spoa-hub"; fi
python3 "$tools/train.py" --hub "$hub" --plugins "$out" --rounds 4 -- "${runner[@]}"
