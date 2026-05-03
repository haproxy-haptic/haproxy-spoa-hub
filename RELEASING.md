# Releasing

This project produces three artifacts on every version tag:

- A multi-arch Docker image for the hub binary (cosign-signed, with a
  CycloneDX SBOM attestation).
- The hub binary and example plugin `.so` files for amd64, arm64, and armv7,
  uploaded to the GitLab Generic Package Registry alongside per-file
  `.sha256` sidecars and `.cosign.bundle` signatures.
- A Rust crate (`haproxy-spoa-hub-plugin-api`) on crates.io.

All three are published automatically by CI when you push a version tag.

## Prerequisites

- A crates.io API token stored as the `CARGO_REGISTRY_TOKEN` CI/CD variable
  (Settings > CI/CD > Variables, masked and protected).
- Permission to push tags to the repository.
- **Generic packages "Reject duplicates" enabled** under
  Settings > Packages and Registries > Generic packages. Without this, a
  re-run of the release pipeline on the same tag silently overwrites the
  bytes uploaded earlier.

## Release process

1. **Update `CHANGELOG.md`**: move items from `[Unreleased]` into a new
   `## [x.y.z]` section.

2. **Bump versions** in `Cargo.toml` files:
   - `crates/hub/Cargo.toml` — always bump for a hub release.
   - `crates/plugin-api/Cargo.toml` — bump only when the plugin API has
     changed. The CI job compares the local version against crates.io and
     skips publishing if they match.

3. **Commit and tag**:

   ```sh
   git add -A && git commit -m "release: v0.2.0"
   git tag v0.2.0
   git push origin main v0.2.0
   ```

4. **CI handles the rest**:
   - Cross-compiles for amd64, arm64, and armv7 with `cargo auditable`
     (binaries embed a compressed `Cargo.lock`-equivalent so SBOM tools can
     enumerate Rust deps without source access)
   - Generates per-file `.sha256` sidecars and a combined `SHA256SUMS`
   - Cosign keyless-signs every blob (binary + `.so`) using the GitLab OIDC
     id token, producing `.cosign.bundle` files
   - Uploads everything to the Generic Package Registry under
     `packages/generic/spoa-hub/<tag>/`
   - Builds and pushes a multiarch Docker image to the GitLab container
     registry (`registry.gitlab.com/haproxy-haptic/haproxy-spoa-hub`),
     cosign-signs it by digest, and attaches a CycloneDX SBOM attestation
   - Publishes the plugin API crate to crates.io (if the version changed)
   - Creates a GitLab release with all artifacts linked as release assets

## Verifying a release

After CI finishes, verify the release end-to-end from a clean machine:

```sh
make verify-release VERSION=v0.2.1
```

This downloads `SHA256SUMS`, every artifact named in it, and every artifact's
cosign bundle from the package registry, then runs `sha256sum -c` and
`cosign verify-blob` against the project's tag identity.

## Version coordination

The hub and plugin API crate are versioned independently. A hub-only fix
(e.g., a performance improvement in dispatch) does not require an API crate
bump. Conversely, adding a new method to `SpoePlugin` or a variant to
`SpoeValue` requires bumping the API crate.

When both change in the same release, use the same version number for
simplicity.

## First-time publish

The very first publish to crates.io must be done manually because the
`publish-crate` CI job runs only on `v*` tags:

```sh
cargo publish -p haproxy-spoa-hub-plugin-api --dry-run  # verify first
cargo publish -p haproxy-spoa-hub-plugin-api
```

## Prerelease tags

Tags containing `-alpha`, `-beta`, or `-rc` (e.g., `v0.2.0-rc1`) will
build and push Docker images but will not tag them as `latest`.

## Yanking a crate version

If a published version has a critical bug:

```sh
cargo yank --vers x.y.z haproxy-spoa-hub-plugin-api
```

This prevents new projects from depending on it but does not break existing
lockfiles.
