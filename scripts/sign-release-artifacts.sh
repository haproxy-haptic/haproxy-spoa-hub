#!/usr/bin/env bash
# Cosign-sign every release artifact under ./artifacts using the GitLab OIDC
# id token from $SIGSTORE_ID_TOKEN (keyless). Produces <file>.cosign.bundle
# next to each input file.
#
# Inputs (env):
#   SIGSTORE_ID_TOKEN  — GitLab id_tokens JWT with aud=sigstore (CI provides)
#
# Skips files matching *.sha256, SHA256SUMS, and existing *.cosign.bundle.

set -euo pipefail

: "${SIGSTORE_ID_TOKEN:?SIGSTORE_ID_TOKEN not set; configure id_tokens in the CI job}"

cd artifacts

shopt -s nullglob
for f in *; do
    case "${f}" in
        *.sha256 | *.cosign.bundle | SHA256SUMS) continue ;;
    esac
    cosign sign-blob --yes "${f}" --bundle "${f}.cosign.bundle"
done
