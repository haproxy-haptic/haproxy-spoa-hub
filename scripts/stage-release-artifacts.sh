#!/bin/sh
# Stage release artifacts: copy per-arch built binaries into ./artifacts with
# versioned filenames matching the published-asset naming convention, and emit
# a per-file .sha256 sidecar for each.
#
# Inputs (env):
#   ARCH           — target arch (amd64 | arm64 | armv7), used in filenames
#   GLIBC_VERSION  — glibc version baseline (e.g. 2.36), used in filenames
#
# Reads from binaries/${ARCH}/ (produced by cargo build) and writes to
# artifacts/. Idempotent — safe to re-run.

set -eu

: "${ARCH:?ARCH not set}"
: "${GLIBC_VERSION:?GLIBC_VERSION not set}"

SUFFIX="-${ARCH}-glibc${GLIBC_VERSION}"
SRC="binaries/${ARCH}"
DST="artifacts"

mkdir -p "${DST}"

# Hub binary (no .so)
cp "${SRC}/haproxy-spoa-hub" "${DST}/haproxy-spoa-hub${SUFFIX}"

# Example plugins (.so files)
for plugin in noop echo delay; do
    cp "${SRC}/lib${plugin}_plugin.so" "${DST}/lib${plugin}_plugin${SUFFIX}.so"
done

# Per-file SHA256 sidecars
cd "${DST}"
for f in *"${SUFFIX}"*; do
    [ -f "$f" ] || continue
    case "$f" in
        *.sha256) continue ;;
    esac
    sha256sum "$f" > "$f.sha256"
done
