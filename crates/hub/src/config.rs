use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
}

fn default_timeout() -> u64 {
    100
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    ReadFile(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("validation error: {0}")]
    Validation(String),
}

impl HubConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
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

        if !self.plugin_dir.exists() {
            return Err(ConfigError::Validation(format!(
                "plugin_dir '{}' does not exist",
                self.plugin_dir.display()
            )));
        }

        let mut names = HashSet::new();
        for plugin in &self.plugins {
            if !names.insert(&plugin.name) {
                return Err(ConfigError::Validation(format!(
                    "duplicate plugin name '{}'",
                    plugin.name
                )));
            }
        }

        for plugin in &self.plugins {
            let lib_path = self.plugin_dir.join(&plugin.library);
            if !lib_path.exists() {
                return Err(ConfigError::Validation(format!(
                    "plugin '{}': library '{}' not found in '{}'",
                    plugin.name,
                    plugin.library,
                    self.plugin_dir.display()
                )));
            }
        }

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

        for plugin in &self.plugins {
            if let Some(timeout) = plugin.timeout_ms {
                if timeout == 0 {
                    return Err(ConfigError::Validation(format!(
                        "plugin '{}': timeout_ms must be positive",
                        plugin.name
                    )));
                }
            }
        }

        if self.default_timeout_ms == 0 {
            return Err(ConfigError::Validation(
                "default_timeout_ms must be positive".into(),
            ));
        }

        Ok(())
    }

    fn check_circular_dependencies(&self) -> Result<(), ConfigError> {
        let name_to_deps: HashMap<String, Vec<String>> = self
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.depends_on.clone()))
            .collect();

        let mut visited = HashSet::new();
        let mut in_stack = HashSet::new();

        for plugin in &self.plugins {
            if !visited.contains(&plugin.name) {
                Self::dfs_cycle_check(&plugin.name, &name_to_deps, &mut visited, &mut in_stack)?;
            }
        }
        Ok(())
    }

    fn dfs_cycle_check(
        node: &str,
        deps: &HashMap<String, Vec<String>>,
        visited: &mut HashSet<String>,
        in_stack: &mut HashSet<String>,
    ) -> Result<(), ConfigError> {
        visited.insert(node.to_string());
        in_stack.insert(node.to_string());

        if let Some(node_deps) = deps.get(node) {
            for dep in node_deps {
                if !visited.contains(dep.as_str()) {
                    Self::dfs_cycle_check(dep, deps, visited, in_stack)?;
                } else if in_stack.contains(dep.as_str()) {
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
        std::fs::write(dir.path().join("libnoop.so"), b"fake").unwrap();

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
        std::fs::write(dir.path().join("liba.so"), b"fake").unwrap();

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
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_invalid_depends_on_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("liba.so"), b"fake").unwrap();

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
