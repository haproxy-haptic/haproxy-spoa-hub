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
    /// Typed parameters from the `[plugins.params]` TOML table.
    /// Supports nested tables, arrays, and all native TOML value types.
    #[serde(default)]
    pub params: toml::Table,
}

fn default_timeout() -> u64 {
    100
}

fn default_blocking_keep_alive() -> u64 {
    300
}

fn default_log_level() -> String {
    "info".into()
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
        let config: Self = toml::from_str(&content)?;
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
