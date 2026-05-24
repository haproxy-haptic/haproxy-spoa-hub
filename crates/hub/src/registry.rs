use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;

use crate::plugin::Plugin;
use crate::plugin_loader::LoadedPlugin;

/// Build a [`SharedRegistry`] from a list of loaded plugins.
///
/// Convenience constructor that combines [`PluginRegistry::build`] with
/// the `Arc<ArcSwap<…>>` wrapping needed for runtime use.
pub fn shared_registry(loaded: Vec<LoadedPlugin>) -> Result<SharedRegistry, RegistryError> {
    Ok(Arc::new(ArcSwap::from_pointee(PluginRegistry::build(
        loaded,
    )?)))
}

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
    pub instance: Arc<Plugin>,
    pub timeout: Duration,
}

/// Registry of loaded plugins and their execution plans per message.
pub struct PluginRegistry {
    plugins: HashMap<String, PluginEntry>,
    message_plans: HashMap<String, ExecutionPlan>,
    /// Reverse dependency map: plugin name → plugins that depend on it.
    reverse_deps: HashMap<String, Vec<String>>,
}

impl PluginRegistry {
    /// Build a registry from loaded plugins.
    ///
    /// Computes execution plans for each SPOE message based on
    /// plugin dependency declarations.
    pub fn build(loaded: Vec<LoadedPlugin>) -> Result<Self, RegistryError> {
        let mut plugins = HashMap::with_capacity(loaded.len());
        let mut message_to_plugins: HashMap<_, Vec<String>> = HashMap::with_capacity(loaded.len());
        let mut depends_on_map = HashMap::with_capacity(loaded.len());
        let mut reverse_deps: HashMap<_, Vec<String>> = HashMap::with_capacity(loaded.len());

        for plugin in loaded {
            let LoadedPlugin {
                name,
                instance,
                messages,
                depends_on,
                timeout,
                ..
            } = plugin;
            for msg in messages {
                message_to_plugins
                    .entry(msg)
                    .or_default()
                    .push(name.clone());
            }
            // Build reverse dependency map inline: for each plugin,
            // record which plugins depend on it. Used by skip-cascade logic.
            for dep in &depends_on {
                reverse_deps
                    .entry(dep.clone())
                    .or_default()
                    .push(name.clone());
            }
            depends_on_map.insert(name.clone(), depends_on);
            plugins.insert(name, PluginEntry { instance, timeout });
        }

        let mut message_plans = HashMap::with_capacity(message_to_plugins.len());
        for (msg, plugin_names) in message_to_plugins {
            let plan = build_execution_plan(&plugin_names, &depends_on_map).map_err(|reason| {
                RegistryError::ExecutionPlan {
                    message: msg.clone(),
                    reason,
                }
            })?;
            message_plans.insert(msg, plan);
        }

        Ok(Self {
            plugins,
            message_plans,
            reverse_deps,
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

    /// Get plugins that directly depend on the given plugin.
    #[must_use]
    pub fn dependents_of(&self, name: &str) -> &[String] {
        self.reverse_deps.get(name).map_or(&[], Vec::as_slice)
    }

    /// Call `shutdown()` on all loaded plugins.
    pub fn shutdown_all(&self) {
        for (name, entry) in &self.plugins {
            tracing::info!(plugin = %name, "shutting down plugin");
            entry.instance.shutdown();
        }
    }

    /// Call `drain()` on every loaded plugin sequentially, sharing a
    /// single wall-clock budget across the set. Returns one
    /// `(plugin_name, drained_cleanly)` entry per plugin so the caller
    /// can record metrics and ERROR-log the names that hit the
    /// deadline.
    ///
    /// Sharing a single deadline (rather than allocating `timeout`
    /// per plugin) means a slow plugin doesn't get to eat the full
    /// budget while leaving zero for everyone else — but it also
    /// means later plugins in the iteration get less time than
    /// earlier ones. That asymmetry is fine in practice: the
    /// expected steady state is ≤2 plugins with in-flight state per
    /// deployment (mirror, maybe otel) and either both have headroom
    /// or only one is busy. Iteration order across the registry's
    /// `HashMap` is NOT stable — operators who need a specific
    /// priority can't control it by re-ordering `[plugins.*]`
    /// blocks. A misbehaving plugin doesn't get to eat into another
    /// plugin's budget because the operator can always raise
    /// `[hub] reload_drain_timeout_ms`.
    ///
    /// Plugins with no in-flight state (v1/v2 synchronous majority,
    /// and any v3+ plugin that hasn't overridden `drain`) return
    /// immediately and don't consume budget.
    #[must_use]
    pub fn drain_all(&self, total_timeout: std::time::Duration) -> Vec<(String, bool)> {
        let deadline = std::time::Instant::now() + total_timeout;
        self.plugins
            .iter()
            .map(|(name, entry)| {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                tracing::debug!(
                    plugin = %name,
                    remaining_ms = remaining.as_millis(),
                    "draining plugin"
                );
                let drained = entry.instance.drain(remaining);
                if !drained {
                    tracing::error!(
                        plugin = %name,
                        total_timeout_ms = total_timeout.as_millis(),
                        "plugin did not drain within deadline; in-flight work will be lost on shutdown"
                    );
                }
                (name.clone(), drained)
            })
            .collect()
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
    let name_set: HashSet<&str> = plugin_names.iter().map(String::as_str).collect();

    // Filter dependencies to only those relevant to this message's plugins
    let mut in_degree: HashMap<&str, usize> = HashMap::with_capacity(plugin_names.len());
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::with_capacity(plugin_names.len());

    for name in plugin_names {
        let degree = in_degree.entry(name.as_str()).or_insert(0);
        if let Some(deps) = depends_on.get(name.as_str()) {
            for dep in deps {
                let dep_str = dep.as_str();
                if name_set.contains(dep_str) {
                    *degree += 1;
                    dependents.entry(dep_str).or_default().push(name.as_str());
                }
            }
        }
    }

    let mut groups = Vec::with_capacity(plugin_names.len());
    let mut remaining = in_degree;

    loop {
        // Collect all plugins with zero in-degree
        let ready: Vec<&str> = remaining
            .iter()
            .filter_map(|(&name, &deg)| (deg == 0).then_some(name))
            .collect();

        if ready.is_empty() {
            if remaining.is_empty() {
                break;
            }
            return Err(format!("circular dependency among: {:?}", remaining.keys()));
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
                    name.to_string(),
                    d.iter().map(ToString::to_string).collect(),
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
