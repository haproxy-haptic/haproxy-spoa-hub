//! ABI compatibility matrix.
//!
//! Loads every published plugin version listed in
//! `tests/abi-matrix-versions.toml` against the current hub binary
//! and asserts the expected outcome (compatible / forward /
//! hard-incompatible). Designed to catch the failure modes the
//! hand-rolled vtable migration is meant to prevent:
//!
//! - Accidental field reorder or removal in `PluginVTable`.
//! - Missed `PLUGIN_API_VERSION` bump after appending a field.
//! - `abi_stable` layout drift if the pin is unintentionally relaxed.
//! - Plugin authors forgetting to set `api_version` correctly.
//!
//! Run locally with:
//!
//! ```sh
//! cargo test --workspace --test abi_matrix -- --nocapture
//! ```
//!
//! Run in CI on every MR (catches the regression before merge) and
//! nightly (catches drift introduced by upstream dependency updates).
//!
//! # Bootstrap state
//!
//! At the time the v0.4.0 plugin-api migration lands, no plugins have
//! been published against the new ABI yet, so this test is a no-op.
//! As external plugin migrations land (each adds an entry to
//! `abi-matrix-versions.toml`), the matrix populates and this test
//! starts catching real regressions.
//!
//! # See also
//!
//! - `docs/adr/0001-abi-evolution.md` for the design rationale.
//! - `docs/abi-evolution.md` for the contributor / release-process
//!   reference, including "When the matrix test goes red".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use haproxy_spoa_hub::plugin::{Plugin, PluginLoadError};
use haproxy_spoa_hub_plugin_api::PLUGIN_API_VERSION_V1;

#[derive(Debug, Deserialize)]
struct MatrixSpec {
    /// Per-plugin metadata. Keyed on the plugin name (e.g. `coraza`).
    #[serde(default)]
    plugins: HashMap<String, PluginEntry>,
}

#[derive(Debug, Deserialize)]
struct PluginEntry {
    /// GitLab project path for this plugin's repository, e.g.
    /// `haproxy-haptic/haproxy-spoa-hub-plugin-coraza`. Used to build
    /// the package-registry URL.
    project: String,
    /// Generic-package name as published by the plugin's CI. Usually
    /// matches the plugin's short name (`coraza`, `external-auth`, ...).
    package: String,
    /// `.so` filename inside the package (e.g. `libcoraza_plugin.so`).
    /// May vary per release if the upstream Cargo crate name changes.
    so_filename: String,
    versions: Vec<PluginVersionSpec>,
}

#[derive(Debug, Deserialize)]
struct PluginVersionSpec {
    /// Plugin's own `SemVer` tag (`0.4.0`, `0.5.0`, …). Drives the
    /// package-registry URL path.
    version: String,
    /// `api_version` value the plugin reports. v1-baseline plugins
    /// report `1`. Matrix expects `Plugin::api_version()` to match.
    plugin_api_version: u32,
    /// Expected outcome of loading this plugin version against the
    /// current hub.
    expected_load: ExpectedLoad,
    /// Optional sha256 hex of the `.so`. When set, the cached or
    /// downloaded file is verified before load.
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExpectedLoad {
    /// Plugin loads, v1 baseline methods callable.
    Compatible,
    /// Plugin reports a higher `api_version` than the hub knows about.
    /// Hub uses only the fields it knows; extras are ignored.
    Forward,
    /// Plugin too old / wrong ABI shape. Hub MUST refuse to load
    /// with a clean structured error.
    HardIncompatible,
}

fn matrix_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/abi-matrix-versions.toml")
}

fn load_matrix_spec() -> MatrixSpec {
    let path = matrix_spec_path();
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&content).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Cache directory for downloaded plugin `.so` files.
fn cache_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("abi-matrix-cache");
    std::fs::create_dir_all(&dir).expect("create abi-matrix-cache");
    dir
}

/// Compute SHA-256 of a file as lowercase hex.
fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Download a plugin `.so` from the GitLab Generic Package Registry,
/// caching it locally under `target/abi-matrix-cache/`. If a sha256
/// is provided and the cached file matches, the download is skipped.
///
/// URL shape:
/// `https://gitlab.com/api/v4/projects/<url-encoded-project>/packages/generic/<package>/<version>/<filename>`
fn download_plugin_so(
    plugin_name: &str,
    entry: &PluginEntry,
    spec: &PluginVersionSpec,
) -> Result<PathBuf, String> {
    // One file per (plugin_name, version) — version goes in the filename
    // so multiple plugin versions cache side-by-side.
    let cache_path = cache_dir().join(format!(
        "{plugin_name}__{ver}__{so}",
        ver = spec.version,
        so = entry.so_filename,
    ));

    // Cache hit: skip the download if the on-disk SHA matches the
    // expected one (or if no expected hash is set, just trust cache).
    if cache_path.exists() {
        match (&spec.sha256, sha256_file(&cache_path)) {
            (Some(expected), Ok(actual)) if expected == &actual => return Ok(cache_path),
            (Some(_), Ok(_)) => {
                // Hash drift — fall through and re-download.
                let _ = std::fs::remove_file(&cache_path);
            }
            (Some(_), Err(_)) => {
                // Couldn't read; re-download.
                let _ = std::fs::remove_file(&cache_path);
            }
            (None, _) => return Ok(cache_path),
        }
    }

    let project_enc = urlencoding_minimal(&entry.project);
    let url = format!(
        "https://gitlab.com/api/v4/projects/{project}/packages/generic/{pkg}/{ver}/{so}",
        project = project_enc,
        pkg = entry.package,
        ver = spec.version,
        so = entry.so_filename,
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("reqwest client: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "GET {url} returned HTTP {status}",
            status = resp.status()
        ));
    }

    let bytes = resp
        .bytes()
        .map_err(|e| format!("read body of {url}: {e}"))?;
    std::fs::write(&cache_path, &bytes)
        .map_err(|e| format!("write {}: {e}", cache_path.display()))?;

    if let Some(expected) = &spec.sha256 {
        let actual = sha256_file(&cache_path)?;
        if expected != &actual {
            let _ = std::fs::remove_file(&cache_path);
            return Err(format!(
                "sha256 mismatch for {url}: expected {expected}, got {actual}"
            ));
        }
    }

    Ok(cache_path)
}

/// Minimal RFC-3986-ish URL-encoder for the project path. We only
/// need to encode `/` because GitLab requires URL-encoded project
/// paths for the API. Avoids pulling in another crate.
fn urlencoding_minimal(s: &str) -> String {
    s.replace('/', "%2F")
}

/// Run one matrix cell: load the plugin `.so`, drive it through the
/// expected scenario, assert the outcome. Returns `Ok(())` on
/// success or `Err(reason)` describing the assertion failure.
fn run_matrix_cell(
    plugin_name: &str,
    spec: &PluginVersionSpec,
    so_path: &Path,
) -> Result<(), String> {
    // SAFETY: dlopen of a downloaded .so. We trust the GitLab Package
    // Registry as the source of authoritative plugin builds.
    let load_result = unsafe { Plugin::load(so_path) };

    match (&spec.expected_load, load_result) {
        (ExpectedLoad::Compatible | ExpectedLoad::Forward, Ok(plugin)) => {
            if plugin.api_version() != spec.plugin_api_version {
                return Err(format!(
                    "{plugin_name}@{ver}: api_version mismatch: \
                     plugin reports {got}, toml says {expected}",
                    ver = spec.version,
                    got = plugin.api_version(),
                    expected = spec.plugin_api_version,
                ));
            }
            // v1 baseline accessors must work on every loaded plugin.
            if plugin.name().is_empty() {
                return Err(format!(
                    "{plugin_name}@{ver}: name() returned empty",
                    ver = spec.version
                ));
            }
            if plugin.plugin_version().is_empty() {
                return Err(format!(
                    "{plugin_name}@{ver}: plugin_version() returned empty",
                    ver = spec.version
                ));
            }
            // Drop the plugin: exercises shutdown + destroy + dlclose.
            // A bug in any of those three would surface here as a
            // segfault or a sanitizer hit (when run under one).
            drop(plugin);
            Ok(())
        }
        (ExpectedLoad::HardIncompatible, Err(_)) => {
            // Expected failure. Could tighten this to assert the
            // specific PluginLoadError variant once we have real
            // hard-incompat samples to test against.
            Ok(())
        }
        (ExpectedLoad::Compatible | ExpectedLoad::Forward, Err(e)) => Err(format!(
            "{plugin_name}@{ver}: expected to load but failed: {e}",
            ver = spec.version
        )),
        (ExpectedLoad::HardIncompatible, Ok(plugin)) => {
            drop(plugin);
            Err(format!(
                "{plugin_name}@{ver}: expected hard-incompatible but loaded successfully",
                ver = spec.version
            ))
        }
    }
}

/// Quick load-error type-check so the compiler verifies the import
/// path stays valid even when the matrix is empty (otherwise dead
/// code warnings turn this into a permanent stub).
#[allow(dead_code)]
fn _force_use(_e: PluginLoadError) {}

#[allow(dead_code)]
const _MIN_VERSION: u32 = PLUGIN_API_VERSION_V1;

#[test]
fn abi_matrix() {
    let spec = load_matrix_spec();

    if spec.plugins.is_empty() {
        eprintln!(
            "abi_matrix: tests/abi-matrix-versions.toml has no plugin \
             entries yet (expected during the v0.4.0 plugin-api \
             migration window). Once plugins publish against \
             plugin-api 0.4.0+, append their entries to the toml. \
             Skipping."
        );
        return;
    }

    let mut failures = Vec::new();
    let mut report_lines = Vec::new();
    for (plugin_name, entry) in &spec.plugins {
        for version_spec in &entry.versions {
            let cell_label = format!("{plugin_name}@{}", version_spec.version);
            let so_path = match download_plugin_so(plugin_name, entry, version_spec) {
                Ok(p) => p,
                Err(reason) => {
                    failures.push(format!("{cell_label}: download failed: {reason}"));
                    report_lines.push(format!("{cell_label}: download FAIL"));
                    continue;
                }
            };
            match run_matrix_cell(plugin_name, version_spec, &so_path) {
                Ok(()) => report_lines.push(format!(
                    "{cell_label}: {kind:?} OK",
                    kind = version_spec.expected_load
                )),
                Err(reason) => {
                    report_lines.push(format!("{cell_label}: FAIL ({reason})"));
                    failures.push(reason);
                }
            }
        }
    }

    eprintln!("\nabi_matrix report:");
    for line in &report_lines {
        eprintln!("  {line}");
    }
    eprintln!();

    if !failures.is_empty() {
        let report = failures.join("\n  - ");
        panic!("abi_matrix had failures:\n  - {report}");
    }
}
