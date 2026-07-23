pub mod adaptive;
pub mod config;
pub mod config_watcher;
pub mod connection;
pub mod crash_handler;
pub mod dispatch;
pub mod listener;
pub mod metrics;
pub mod plugin;
pub mod plugin_loader;
pub mod plugin_metrics;
pub mod registry;
pub mod validate;

/// Test helpers shared across unit tests, integration tests, and benchmarks.
///
/// Always compiled so that integration tests and benchmarks (which are
/// separate crates and cannot see `#[cfg(test)]` items) can import
/// `workspace_target_dir()` without duplicating it.
#[doc(hidden)]
pub mod test_support;
