use std::{
    collections::{HashMap, HashSet},
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct HubConfig {
    pub listeners: Vec<ListenerConfig>,
    pub plugin_dir: PathBuf,
    #[serde(default = "default_timeout")]
    pub default_timeout_ms: u64,
    pub metrics_addr: Option<SocketAddr>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Tokio worker threads. Defaults to the number of CPU cores.
    pub worker_threads: Option<usize>,
    /// Maximum concurrent SPOP connections. Defaults to unlimited.
    pub max_connections: Option<usize>,
    /// How long idle blocking pool threads are kept alive (seconds).
    /// Longer values avoid thread re-creation overhead on bursty traffic.
    /// Defaults to 300 (5 minutes). Tokio's default is 10 seconds.
    #[serde(default = "default_blocking_keep_alive")]
    pub blocking_thread_keep_alive_secs: u64,
    /// Maximum number of Tokio blocking-pool threads. When omitted, the hub
    /// derives a finite bound by summing every plugin's resolved concurrency.
    /// This is defense in depth; per-plugin admission gates remain the primary
    /// overload control.
    pub max_blocking_threads: Option<usize>,
    /// How long to wait for in-flight plugin work to drain after a
    /// registry swap before forcing shutdown. `None` (the default)
    /// derives the budget from `max(plugin.timeout_ms) * 1.5`, floored
    /// at `1000ms` and capped at `30_000ms`. Set explicitly when an
    /// operator's plugin set has long-tail dispatch latencies the
    /// per-plugin `timeout_ms` doesn't fully capture (e.g. mirror's
    /// HTTP fan-out completing well after the SPOE ACK).
    ///
    /// Set to 0 to opt out of draining entirely (legacy behaviour:
    /// swap → shutdown immediately, losing in-flight fire-and-forget
    /// work). Not recommended.
    pub reload_drain_timeout_ms: Option<u64>,
    pub plugins: Vec<PluginConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ListenerConfig {
    #[serde(rename = "type")]
    pub listener_type: ListenerType,
    pub address: String,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ListenerType {
    Unix,
    Tcp,
}

#[derive(Debug, Deserialize)]
pub struct PluginConfig {
    pub name: String,
    pub library: String,
    pub messages: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub timeout_ms: Option<u64>,
    /// Maximum calls from this plugin that may execute concurrently. Defaults
    /// to the process's available parallelism.
    pub max_concurrency: Option<usize>,
    /// Number of calls allowed to wait for a concurrency permit. Zero (the
    /// default) rejects excess work immediately instead of building latency.
    #[serde(default)]
    pub max_queue: usize,
    /// Maximum queue wait. When omitted, queued calls share the plugin's total
    /// timeout budget. Has no effect while `max_queue` is zero.
    pub queue_timeout_ms: Option<u64>,
    /// Opt into the latency-feedback concurrency controller (ADR-0002). When
    /// true, `max_concurrency` becomes the controller's ceiling rather than a
    /// fixed limit, and the semaphore is resized at runtime to track the
    /// plugin's service time. Off by default (fixed `max_concurrency`).
    #[serde(default)]
    pub adaptive_concurrency: bool,
    /// Typed parameters from the `[plugins.params]` TOML table.
    /// Supports nested tables, arrays, and all native TOML value types.
    #[serde(default)]
    pub params: toml::Table,
}

fn default_timeout() -> u64 {
    100
}

/// Resolve the drain budget for `apply_reload`. Returns the explicit
/// `[hub] reload_drain_timeout_ms` value if set; otherwise derives
/// `max(plugin.timeout_ms) * 1.5`, clamped to `[1000ms, 30_000ms]`.
///
/// Why 1.5×: plugins with fire-and-forget background work
/// (mirror, otel) typically finish their HTTP dispatch within their
/// declared SPOE timeout — the timeout is the SPOE-side budget, but
/// the actual upstream call can land slightly after the ACK was sent.
/// 1.5× covers the common tail without making reloads slow.
///
/// Why floor `1000ms`: even synchronous plugins might be doing a
/// blocking I/O in `process()` that's already in flight at swap time
/// (rare but observed in coraza when `SecRules` with `pause:N`-style
/// directives are loaded). 1s gives them time to return.
///
/// Why ceiling `30_000ms`: a misconfigured plugin shouldn't be able to
/// block reloads indefinitely. 30s is well past any legitimate
/// upstream-call timeout and short enough that operators waiting on
/// a reload see one within a reasonable SLO.
#[must_use]
pub fn resolve_reload_drain_timeout(cfg: &HubConfig) -> std::time::Duration {
    use std::time::Duration;
    if let Some(explicit) = cfg.reload_drain_timeout_ms {
        return Duration::from_millis(explicit);
    }
    let max_plugin = cfg
        .plugins
        .iter()
        .map(|p| p.timeout_ms.unwrap_or(cfg.default_timeout_ms))
        .max()
        .unwrap_or(cfg.default_timeout_ms);
    // (max_plugin * 3) / 2 = 1.5× without floats.
    let derived = max_plugin.saturating_mul(3) / 2;
    let clamped = derived.clamp(1_000, 30_000);
    Duration::from_millis(clamped)
}

fn default_blocking_keep_alive() -> u64 {
    300
}

fn default_log_level() -> String {
    "info".into()
}

/// Resolve one plugin's concurrency without relying on Tokio's much larger
/// blocking-pool default. `available_parallelism` is fallible in restricted
/// containers; one worker is always safe.
#[must_use]
pub fn resolve_plugin_concurrency(plugin: &PluginConfig) -> usize {
    plugin
        .max_concurrency
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
}

/// The most concurrency a plugin may ever hold. For an adaptive plugin
/// (ADR-0002) that is the controller's ceiling (`AdaptiveConfig::max_limit`),
/// which the semaphore can grow up to; otherwise the fixed resolved
/// concurrency. Sizing the blocking pool to the sum of ceilings keeps a grown
/// adaptive semaphore from overflowing into Tokio's unmetered blocking queue.
#[must_use]
pub fn resolve_plugin_ceiling(plugin: &PluginConfig) -> usize {
    if plugin.adaptive_concurrency {
        crate::adaptive::AdaptiveConfig::resolve(plugin.max_concurrency, &plugin.params).max_limit
    } else {
        resolve_plugin_concurrency(plugin)
    }
}

/// Resolve the process-wide blocking-pool cap. The derived value permits every
/// plugin's admitted calls to execute without entering Tokio's internal,
/// unmetered blocking queue. Operators should assign per-plugin concurrency as
/// resource budgets; the explicit override is primarily restart-time defense
/// in depth and must not be lower than the sum if queue-free dispatch matters.
#[must_use]
pub fn resolve_max_blocking_threads(config: &HubConfig) -> usize {
    config.max_blocking_threads.unwrap_or_else(|| {
        config
            .plugins
            .iter()
            .map(resolve_plugin_ceiling)
            .fold(0usize, usize::saturating_add)
            .max(1)
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    ReadFile(#[from] io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("validation error: {0}")]
    Validation(String),
}

impl HubConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        Self::load_from_str(&content)
    }

    /// Parse + validate a config from a TOML string already held in
    /// memory. Used by the reload path so the bytes the dedup check
    /// hashed are guaranteed to be the same bytes the registry was
    /// built from — without this, a write landing between the dedup
    /// read and the reload read would cache stale bytes against a
    /// freshly-loaded registry, and the NEXT reload's dedup decision
    /// would compare against the wrong baseline. (Gitar review on
    /// hub MR !163.)
    pub fn load_from_str(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.listeners.is_empty() {
            return Err(ConfigError::Validation(
                "at least one listener must be configured".into(),
            ));
        }

        if self.default_timeout_ms == 0 {
            return Err(ConfigError::Validation(
                "default_timeout_ms must be positive".into(),
            ));
        }

        if self.max_blocking_threads == Some(0) {
            return Err(ConfigError::Validation(
                "max_blocking_threads must be positive".into(),
            ));
        }

        if !self.plugin_dir.is_dir() {
            return Err(ConfigError::Validation(format!(
                "plugin_dir '{}' is not a directory",
                self.plugin_dir.display()
            )));
        }

        let mut names = HashSet::with_capacity(self.plugins.len());
        for plugin in &self.plugins {
            if !names.insert(&plugin.name) {
                return Err(ConfigError::Validation(format!(
                    "duplicate plugin name '{}'",
                    plugin.name
                )));
            }

            if !self.plugin_dir.join(&plugin.library).is_file() {
                return Err(ConfigError::Validation(format!(
                    "plugin '{}': library '{}' is not a file in '{}'",
                    plugin.name,
                    plugin.library,
                    self.plugin_dir.display()
                )));
            }

            if plugin.timeout_ms == Some(0) {
                return Err(ConfigError::Validation(format!(
                    "plugin '{}': timeout_ms must be positive",
                    plugin.name
                )));
            }

            if plugin.max_concurrency == Some(0) {
                return Err(ConfigError::Validation(format!(
                    "plugin '{}': max_concurrency must be positive",
                    plugin.name
                )));
            }

            if plugin.queue_timeout_ms == Some(0) {
                return Err(ConfigError::Validation(format!(
                    "plugin '{}': queue_timeout_ms must be positive when set",
                    plugin.name
                )));
            }
        }

        if let Some(configured) = self.max_blocking_threads {
            let admitted = self
                .plugins
                .iter()
                .map(resolve_plugin_ceiling)
                .fold(0usize, usize::saturating_add)
                .max(1);
            if configured < admitted {
                return Err(ConfigError::Validation(format!(
                    "max_blocking_threads ({configured}) must be at least the sum of resolved plugin concurrency ceilings ({admitted})"
                )));
            }
        }

        // Dependency validation in a separate pass since all names must
        // be collected first to check forward references.
        for plugin in &self.plugins {
            for dep in &plugin.depends_on {
                if !names.contains(dep) {
                    return Err(ConfigError::Validation(format!(
                        "plugin '{}': depends_on '{}' which is not a configured plugin",
                        plugin.name, dep
                    )));
                }
            }
        }

        // Check for circular dependencies via topological sort
        self.check_circular_dependencies()?;

        Ok(())
    }

    fn check_circular_dependencies(&self) -> Result<(), ConfigError> {
        let name_to_deps: HashMap<&str, Vec<&str>> = self
            .plugins
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.depends_on.iter().map(String::as_str).collect(),
                )
            })
            .collect();

        let mut visited = HashSet::with_capacity(self.plugins.len());
        let mut in_stack = HashSet::with_capacity(self.plugins.len());

        for plugin in &self.plugins {
            if !visited.contains(plugin.name.as_str()) {
                Self::dfs_cycle_check(&plugin.name, &name_to_deps, &mut visited, &mut in_stack)?;
            }
        }
        Ok(())
    }

    fn dfs_cycle_check<'a>(
        node: &'a str,
        deps: &HashMap<&'a str, Vec<&'a str>>,
        visited: &mut HashSet<&'a str>,
        in_stack: &mut HashSet<&'a str>,
    ) -> Result<(), ConfigError> {
        visited.insert(node);
        in_stack.insert(node);

        if let Some(node_deps) = deps.get(node) {
            for &dep in node_deps {
                if !visited.contains(dep) {
                    Self::dfs_cycle_check(dep, deps, visited, in_stack)?;
                } else if in_stack.contains(dep) {
                    return Err(ConfigError::Validation(format!(
                        "circular dependency detected involving plugin '{node}' and '{dep}'"
                    )));
                }
            }
        }

        in_stack.remove(node);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> (NamedTempFile, PathBuf) {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        let path = f.path().to_path_buf();
        (f, path)
    }

    #[test]
    fn test_valid_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();

        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"noop\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n",
            dir.path().display()
        );
        let (_f, path) = write_config(&cfg);
        let result = HubConfig::load(&path);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    // Reload-dedup TOCTOU fix (Gitar review on !163): the from-str
    // loader has to produce a fully-validated HubConfig from an
    // in-memory buffer so the reload path can dedup-check and load
    // from the same bytes without a second filesystem read. Pin that
    // both happy and invalid inputs route through the same validation
    // logic as the path-based loader.
    #[test]
    fn test_load_from_str_parses_valid_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();
        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"noop\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n",
            dir.path().display()
        );
        let result = HubConfig::load_from_str(&cfg);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn test_load_from_str_runs_same_validation_as_load() {
        // Empty listeners should fail validation regardless of which
        // entry point we used — pin that load_from_str isn't an
        // "unvalidated" shortcut.
        let dir = tempfile::tempdir().unwrap();
        let cfg = format!(
            "listeners = []\nplugin_dir = \"{}\"\nplugins = []\n",
            dir.path().display()
        );
        let err = HubConfig::load_from_str(&cfg).unwrap_err();
        assert!(err.to_string().contains("at least one listener"));
    }

    #[test]
    fn test_load_from_str_surfaces_toml_parse_errors() {
        let err = HubConfig::load_from_str("not valid = = toml ===").unwrap_err();
        // toml::de::Error is wrapped via #[from]; just ensure it's not
        // a Validation error.
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn test_missing_listeners_fails() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = format!(
            "listeners = []\nplugin_dir = \"{}\"\nplugins = []\n",
            dir.path().display()
        );
        let (_f, path) = write_config(&cfg);
        let err = HubConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("at least one listener"));
    }

    #[test]
    fn test_duplicate_plugin_name_fails() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("liba.so"), b"fake").unwrap();

        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"dup\"\n\
             library = \"liba.so\"\n\
             messages = [\"check\"]\n\
             [[plugins]]\n\
             name = \"dup\"\n\
             library = \"liba.so\"\n\
             messages = [\"check\"]\n",
            dir.path().display()
        );
        let (_f, path) = write_config(&cfg);
        let err = HubConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate plugin name"));
    }

    #[test]
    fn test_nonexistent_plugin_dir_fails() {
        let cfg = "plugin_dir = \"/nonexistent/path\"\n\
                   plugins = []\n\
                   [[listeners]]\n\
                   type = \"unix\"\n\
                   address = \"/tmp/test.sock\"\n";
        let (_f, path) = write_config(cfg);
        let err = HubConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("is not a directory"));
    }

    #[test]
    fn test_plugin_params_parsed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libgeoip.so"), b"fake").unwrap();

        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"geoip\"\n\
             library = \"libgeoip.so\"\n\
             messages = [\"check\"]\n\
             [plugins.params]\n\
             db_path = \"/data/GeoLite2.mmdb\"\n\
             locale = \"en\"\n",
            dir.path().display()
        );
        let (_f, path) = write_config(&cfg);
        let config = HubConfig::load(&path).unwrap();
        let plugin = &config.plugins[0];
        assert_eq!(
            plugin.params.get("db_path").unwrap(),
            &toml::Value::String("/data/GeoLite2.mmdb".into())
        );
        assert_eq!(
            plugin.params.get("locale").unwrap(),
            &toml::Value::String("en".into())
        );
    }

    #[test]
    fn test_plugin_params_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();

        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"noop\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n",
            dir.path().display()
        );
        let (_f, path) = write_config(&cfg);
        let config = HubConfig::load(&path).unwrap();
        assert!(config.plugins[0].params.is_empty());
    }

    #[test]
    fn test_plugin_admission_controls_parse_and_resolve() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();
        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             max_blocking_threads = 7\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"noop\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n\
             max_concurrency = 3\n\
             max_queue = 4\n\
             queue_timeout_ms = 2\n",
            dir.path().display()
        );

        let config = HubConfig::load_from_str(&cfg).unwrap();
        assert_eq!(resolve_plugin_concurrency(&config.plugins[0]), 3);
        assert_eq!(config.plugins[0].max_queue, 4);
        assert_eq!(config.plugins[0].queue_timeout_ms, Some(2));
        assert_eq!(resolve_max_blocking_threads(&config), 7);
    }

    #[test]
    fn test_blocking_pool_defaults_to_sum_of_plugin_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();
        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"small\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n\
             max_concurrency = 2\n\
             [[plugins]]\n\
             name = \"large\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n\
             max_concurrency = 5\n",
            dir.path().display()
        );

        let config = HubConfig::load_from_str(&cfg).unwrap();
        assert_eq!(resolve_max_blocking_threads(&config), 7);
    }

    #[test]
    fn test_explicit_blocking_pool_cannot_undercut_admitted_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();
        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             max_blocking_threads = 6\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"small\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n\
             max_concurrency = 2\n\
             [[plugins]]\n\
             name = \"large\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n\
             max_concurrency = 5\n",
            dir.path().display()
        );

        let error = HubConfig::load_from_str(&cfg).expect_err("undersized pool must fail");
        assert!(error.to_string().contains("must be at least the sum"));
    }

    #[test]
    fn test_zero_plugin_concurrency_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();
        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"noop\"\n\
             library = \"libnoop.so\"\n\
             messages = [\"check\"]\n\
             max_concurrency = 0\n",
            dir.path().display()
        );

        let err = HubConfig::load_from_str(&cfg).unwrap_err();
        assert!(err.to_string().contains("max_concurrency must be positive"));
    }

    #[test]
    fn test_invalid_depends_on_fails() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("liba.so"), b"fake").unwrap();

        let cfg = format!(
            "plugin_dir = \"{}\"\n\
             [[listeners]]\n\
             type = \"unix\"\n\
             address = \"/tmp/test.sock\"\n\
             [[plugins]]\n\
             name = \"a\"\n\
             library = \"liba.so\"\n\
             messages = [\"check\"]\n\
             depends_on = [\"nonexistent\"]\n",
            dir.path().display()
        );
        let (_f, path) = write_config(&cfg);
        let err = HubConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }
}
