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

1. **Run the ABI compat matrix locally** (when bumping plugin-api):

   ```sh
   cargo test --workspace --test abi_matrix
   ```

   This loads every previously-published plugin `.so` against the
   current hub binary and asserts the expected outcome (compatible /
   forward / hard-incompatible). If it goes red, do NOT release —
   investigate per `docs/abi-evolution.md`'s "When the matrix test
   goes red" checklist. Common causes: an unintended field reorder in
   `vtable::PluginVTable`, a missed `PLUGIN_API_VERSION` bump after
   appending, or an `abi_stable` pin drift.

2. **Audit `crates/plugin-api/src/vtable.rs`** for layout discipline:
   - Every change since the last release MUST be append-only below
     the v1 baseline marker comment.
   - `PLUGIN_API_VERSION` MUST have bumped by 1 per appended field.
   - Existing field types and order MUST be unchanged.

3. **Audit the `abi_stable` pin**: `grep abi_stable crates/*/Cargo.toml`
   should still show `=0.11.3` (or whatever the current pinned exact
   version is). If the pin changed, treat it as a coordinated rollout:
   re-publish every plugin built against the new pin, run the matrix
   against both old AND new published .so sets, and document the
   migration in CHANGELOG.

4. **Update `CHANGELOG.md`**: move items from `[Unreleased]` into a new
   `## [x.y.z]` section.

5. **Bump versions** in `Cargo.toml` files:
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
bump. Conversely, adding a method to `PluginVTable` or a variant to
`SpoeValue` requires bumping the API crate.

When both change in the same release, use the same version number for
simplicity.

### What kind of plugin-api bump is appropriate?

- **Patch** (0.4.0 → 0.4.1): Documentation fixes, internal helpers
  that don't change FFI shape, code style.
- **Minor** (0.4.0 → 0.5.0): Append a method to `PluginVTable`, append
  a variant to `ConfigValue` / `SpoeValue` / `VarScope` (before the
  `__Other` catch-all), append a method/helper on data types. Old
  plugins built against the previous minor still load — this is the
  prize of the v0.4.0 ABI migration.
- **Major** (0.x → 1.0, or 1.0 → 2.0): Any change that breaks the
  append-only / never-reorder invariant. Every existing plugin must
  rebuild. Treat this as a coordinated rollout same as bumping the
  pinned `abi_stable` version. Should be exceedingly rare; if you
  find yourself here, write a new ADR explaining the trigger.

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
