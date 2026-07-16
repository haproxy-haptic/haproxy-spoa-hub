//! Regression test for plugin removal during config reload.
//!
//! A plugin host cannot prove that arbitrary shared-library termination
//! routines are safe. In production, removing the Coraza plugin dropped the
//! final `libloading::Library`, `dlclose` unmapped its embedded Go runtime,
//! and both hub replicas exited with SIGSEGV immediately after logging a
//! successful registry retirement. The hub must retain every successfully
//! opened plugin mapping until process exit.

#![cfg(target_os = "linux")]

use std::path::Path;

use haproxy_spoa_hub::{plugin::Plugin, test_support::workspace_target_dir};

fn process_maps_library(path: &Path) -> bool {
    let maps = std::fs::read_to_string("/proc/self/maps").expect("read /proc/self/maps");
    let path = path.to_string_lossy();
    maps.lines().any(|line| line.contains(path.as_ref()))
}

#[test]
fn plugin_library_remains_mapped_after_last_instance_drops() {
    let so_path = workspace_target_dir().join("libnoop_plugin.so");
    assert!(
        so_path.exists(),
        "{} must be built before this test (run cargo build --workspace)",
        so_path.display()
    );
    let so_path = so_path.canonicalize().expect("canonicalize plugin path");

    assert!(
        !process_maps_library(&so_path),
        "test process unexpectedly started with {} already mapped",
        so_path.display()
    );

    // SAFETY: this is the workspace's own noop plugin build output.
    let plugin = unsafe { Plugin::load(&so_path) }.expect("load noop plugin");
    assert!(
        process_maps_library(&so_path),
        "plugin must be mapped after load"
    );

    // Models removal of the last plugin generation from the active registry.
    // Instance state is destroyed, but code and foreign-runtime mappings must
    // remain resident until this process exits.
    drop(plugin);
    assert!(
        process_maps_library(&so_path),
        "plugin library was unmapped after its last instance dropped"
    );
}
