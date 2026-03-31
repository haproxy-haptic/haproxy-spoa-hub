use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use haproxy_spoa_hub_plugin_api::PluginBox;

use crate::plugin_loader::LoadedPlugin;

/// A shared, atomically swappable plugin registry.
///
/// Wraps the registry in `Arc<ArcSwap<PluginRegistry>>` so that the
/// active registry can be replaced at runtime (e.g., on SIGHUP)
/// without interrupting in-flight connections. Each connection loads
/// the current registry snapshot when processing a message, so a swap
/// takes effect for all subsequent messages without any locking.
pub type SharedRegistry = Arc<ArcSwap<PluginRegistry>>;

/// A group of plugins that can execute in parallel.
pub type ParallelGroup = Vec<String>;

/// Execution plan for a single SPOE message: ordered groups of plugins.
/// Groups execute sequentially; plugins within a group execute in parallel.
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub groups: Vec<ParallelGroup>,
}

/// Runtime plugin entry in the registry.
pub struct PluginEntry {
    #[allow(dead_code)] // Retained for debugging and future SIGHUP reload logging
    pub name: String,
    pub instance: Arc<PluginBox>,
    pub timeout: std::time::Duration,
}

/// Registry of loaded plugins and their execution plans per message.
pub struct PluginRegistry {
    plugins: HashMap<String, PluginEntry>,
    message_plans: HashMap<String, ExecutionPlan>,
}

impl PluginRegistry {
    /// Build a registry from loaded plugins.
    ///
    /// Computes execution plans for each SPOE message based on
    /// plugin dependency declarations.
    pub fn build(loaded: Vec<LoadedPlugin>) -> Result<Self, RegistryError> {
        let mut plugins = HashMap::new();
        let mut message_to_plugins: HashMap<String, Vec<String>> = HashMap::new();
        let mut depends_on_map: HashMap<String, Vec<String>> = HashMap::new();

        for plugin in loaded {
            for msg in &plugin.messages {
                message_to_plugins
                    .entry(msg.clone())
                    .or_default()
                    .push(plugin.name.clone());
            }
            depends_on_map.insert(plugin.name.clone(), plugin.depends_on.clone());
            plugins.insert(
                plugin.name.clone(),
                PluginEntry {
                    name: plugin.name,
                    instance: plugin.instance,
                    timeout: plugin.timeout,
                },
            );
        }

        let mut message_plans = HashMap::new();
        for (msg, plugin_names) in &message_to_plugins {
            let plan = build_execution_plan(plugin_names, &depends_on_map).map_err(|e| {
                RegistryError::ExecutionPlan {
                    message: msg.clone(),
                    reason: e,
                }
            })?;
            message_plans.insert(msg.clone(), plan);
        }

        Ok(Self {
            plugins,
            message_plans,
        })
    }

    /// Get the execution plan for a message, if any plugins are registered.
    #[must_use]
    pub fn plan_for_message(&self, message_name: &str) -> Option<&ExecutionPlan> {
        self.message_plans.get(message_name)
    }

    /// Get a plugin entry by name.
    #[must_use]
    pub fn get_plugin(&self, name: &str) -> Option<&PluginEntry> {
        self.plugins.get(name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("failed to build execution plan for message '{message}': {reason}")]
    ExecutionPlan { message: String, reason: String },
}

/// Build an execution plan by topologically sorting plugins into
/// parallel groups based on their dependency declarations.
fn build_execution_plan(
    plugin_names: &[String],
    depends_on: &HashMap<String, Vec<String>>,
) -> Result<ExecutionPlan, String> {
    let name_set: std::collections::HashSet<&str> =
        plugin_names.iter().map(String::as_str).collect();

    // Filter dependencies to only those relevant to this message's plugins
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for name in plugin_names {
        in_degree.entry(name.as_str()).or_insert(0);
        if let Some(deps) = depends_on.get(name) {
            for dep in deps {
                if name_set.contains(dep.as_str()) {
                    *in_degree.entry(name.as_str()).or_insert(0) += 1;
                    dependents
                        .entry(dep.as_str())
                        .or_default()
                        .push(name.as_str());
                }
            }
        }
    }

    let mut groups = Vec::new();
    let mut remaining: HashMap<&str, usize> = in_degree.clone();

    loop {
        // Collect all plugins with zero in-degree
        let ready: Vec<&str> = remaining
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(name, _)| *name)
            .collect();

        if ready.is_empty() {
            if remaining.is_empty() {
                break;
            }
            return Err(format!(
                "circular dependency among: {:?}",
                remaining.keys().collect::<Vec<_>>()
            ));
        }

        // Remove ready plugins and decrement dependents
        for &name in &ready {
            remaining.remove(name);
            if let Some(deps) = dependents.get(name) {
                for &dep in deps {
                    if let Some(count) = remaining.get_mut(dep) {
                        *count = count.saturating_sub(1);
                    }
                }
            }
        }

        groups.push(ready.into_iter().map(String::from).collect());
    }

    Ok(ExecutionPlan { groups })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_deps(deps: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        deps.iter()
            .map(|(name, d)| {
                (
                    (*name).to_string(),
                    d.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn test_independent_plugins_single_group() {
        let names = vec!["a".into(), "b".into(), "c".into()];
        let deps = make_deps(&[("a", &[]), ("b", &[]), ("c", &[])]);
        let plan = build_execution_plan(&names, &deps).unwrap();
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].len(), 3);
    }

    #[test]
    fn test_linear_dependency_chain() {
        let names = vec!["a".into(), "b".into(), "c".into()];
        let deps = make_deps(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        let plan = build_execution_plan(&names, &deps).unwrap();
        assert_eq!(plan.groups.len(), 3);
        assert_eq!(plan.groups[0], vec!["a"]);
        assert_eq!(plan.groups[1], vec!["b"]);
        assert_eq!(plan.groups[2], vec!["c"]);
    }

    #[test]
    fn test_diamond_dependency() {
        // a → b, a → c, b → d, c → d
        let names = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let deps = make_deps(&[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])]);
        let plan = build_execution_plan(&names, &deps).unwrap();
        assert_eq!(plan.groups.len(), 3);
        assert_eq!(plan.groups[0], vec!["a"]);
        assert_eq!(plan.groups[1].len(), 2); // b and c in parallel
        assert_eq!(plan.groups[2], vec!["d"]);
    }

    #[test]
    fn test_circular_dependency_detected() {
        let names = vec!["a".into(), "b".into()];
        let deps = make_deps(&[("a", &["b"]), ("b", &["a"])]);
        let result = build_execution_plan(&names, &deps);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("circular dependency"));
    }

    #[test]
    fn test_dependency_outside_message_ignored() {
        // Plugin "a" depends on "x", but "x" is not registered for this message
        let names = vec!["a".into(), "b".into()];
        let deps = make_deps(&[("a", &["x"]), ("b", &[])]);
        let plan = build_execution_plan(&names, &deps).unwrap();
        // Both should be in a single parallel group since "x" is external
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].len(), 2);
    }
}
