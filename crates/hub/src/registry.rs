use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use tokio::sync::Semaphore;

use crate::adaptive::{AdaptiveConfig, AdaptiveLimiter};
use crate::metrics::{MessageMetrics, PluginMetrics};
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
    pub admission: Arc<PluginAdmission>,
    /// Metric handles resolved once for this plugin, so the dispatch hot path
    /// records without re-hashing labels or allocating per call.
    pub metrics: Arc<PluginMetrics>,
}

/// Per-plugin admission state shared by every connection and message plan.
/// A permit is moved into the blocking closure, so a timed-out (and therefore
/// detached) `spawn_blocking` call still consumes capacity until the plugin
/// actually returns.
pub struct PluginAdmission {
    pub(crate) permits: Arc<Semaphore>,
    pub(crate) max_queue: usize,
    pub(crate) queue_timeout: Option<Duration>,
    /// Present when the plugin opted into adaptive concurrency (ADR-0002). It
    /// owns the same [`Semaphore`] as `permits` and resizes it from the
    /// per-call service-time feedback on the dispatch completion path.
    pub(crate) adaptive: Option<Arc<AdaptiveLimiter>>,
    queued: AtomicUsize,
}

impl PluginAdmission {
    /// Build admission state. When `adaptive` is `Some`, the semaphore starts at
    /// the controller's `initial` permit count and is resized at runtime;
    /// otherwise it is fixed at `max_concurrency`.
    fn new(
        max_concurrency: usize,
        max_queue: usize,
        queue_timeout: Option<Duration>,
        adaptive: Option<AdaptiveConfig>,
    ) -> Self {
        let (permits, adaptive) = match adaptive {
            Some(cfg) => {
                let permits = Arc::new(Semaphore::new(cfg.initial));
                let limiter = Arc::new(AdaptiveLimiter::new(Arc::clone(&permits), cfg));
                (permits, Some(limiter))
            }
            None => (Arc::new(Semaphore::new(max_concurrency)), None),
        };
        Self {
            permits,
            max_queue,
            queue_timeout,
            adaptive,
            queued: AtomicUsize::new(0),
        }
    }

    /// Reserve one bounded queue slot. A compare-exchange loop prevents a
    /// burst from overshooting the configured cap.
    pub(crate) fn try_enter_queue(&self) -> bool {
        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            if current >= self.max_queue {
                return false;
            }
            match self.queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn leave_queue(&self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

/// Registry of loaded plugins and their execution plans per message.
pub struct PluginRegistry {
    plugins: HashMap<String, PluginEntry>,
    message_plans: HashMap<String, ExecutionPlan>,
    /// Cached metric handles per handled message (same key set as `message_plans`).
    message_metrics: HashMap<String, MessageMetrics>,
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
                max_concurrency,
                max_queue,
                queue_timeout,
                adaptive,
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
            let metrics = Arc::new(PluginMetrics::new(&name));
            plugins.insert(
                name,
                PluginEntry {
                    instance,
                    timeout,
                    admission: Arc::new(PluginAdmission::new(
                        max_concurrency,
                        max_queue,
                        queue_timeout,
                        adaptive,
                    )),
                    metrics,
                },
            );
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

        // Resolve per-message metric handles once for every message the hub is
        // configured to handle, so `handle_notify_frame` records without
        // per-frame label allocation on the hot path.
        let message_metrics = message_plans
            .keys()
            .map(|msg| (msg.clone(), MessageMetrics::new(msg)))
            .collect();

        Ok(Self {
            plugins,
            message_plans,
            message_metrics,
            reverse_deps,
        })
    }

    /// Get the execution plan for a message, if any plugins are registered.
    #[must_use]
    pub fn plan_for_message(&self, message_name: &str) -> Option<&ExecutionPlan> {
        self.message_plans.get(message_name)
    }

    /// Get the cached metric handles for a handled message. `Some` exactly when
    /// [`plan_for_message`](Self::plan_for_message) is `Some` for the same name.
    #[must_use]
    pub fn message_metrics(&self, message_name: &str) -> Option<&MessageMetrics> {
        self.message_metrics.get(message_name)
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

/// Poll interval for the quiesce wait in [`swap_and_retire`]. 1ms keeps
/// the added reload latency negligible against dispatch latencies
/// (plugin timeouts are ≥ millisecond-scale) without busy-spinning.
const QUIESCE_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Report from [`swap_and_retire`]: what happened while retiring the
/// previous registry, per phase, so the caller can record metrics and
/// log the outcome.
#[derive(Debug)]
pub struct RetireReport {
    /// Whether every in-flight dispatch holding the pre-swap registry
    /// snapshot completed within the budget. `false` means the old
    /// plugin set was drained and shut down while at least one NOTIFY
    /// dispatch still held it — that dispatch's fire-and-forget
    /// follow-up work may be lost. (Memory safety is unaffected either
    /// way: the snapshot's `Arc`s keep the old plugins mapped until the
    /// dispatch returns.)
    pub quiesced_cleanly: bool,
    /// Per-plugin results from [`PluginRegistry::drain_all`]. Empty
    /// when the budget was zero (drain opt-out).
    pub drain_results: Vec<(String, bool)>,
    /// Wall-clock time spent retiring the old registry (quiesce wait
    /// plus plugin drain).
    pub retire_duration: Duration,
}

/// Swap `new_registry` in as the active registry and retire the old
/// one: **swap → quiesce → drain → shutdown**.
///
/// 1. **Swap** — atomic. Every NOTIFY frame read after this point
///    dispatches against the new registry.
/// 2. **Quiesce** — wait for in-flight dispatches that loaded the
///    pre-swap snapshot (`connection::handle_notify_frame`'s
///    `load_full()`) to finish. Without this, the drain phase runs
///    before an in-flight dispatch has enqueued its fire-and-forget
///    background work (the mirror plugin's `runtime.spawn`'d HTTP
///    request), observes nothing in flight, and the work silently dies
///    when the old plugin generation drops — haptic's race B
///    (in-flight `send-spoe-group` NOTIFYs lost around a reload).
/// 3. **Drain** — [`PluginRegistry::drain_all`] with the remaining
///    budget, so plugins' background work enqueued by the (now
///    completed) dispatches gets to finish.
/// 4. **Shutdown** — [`PluginRegistry::shutdown_all`] on the old set.
///
/// The quiesce wait polls the old registry `Arc`'s strong count: the
/// caller-held handle returned by the swap is the only reference that
/// is *not* an in-flight dispatch snapshot (the `ArcSwap` gave up its
/// own reference in the swap, and connections never keep a snapshot
/// beyond a single NOTIFY frame), so a strong count of 1 means every
/// dispatch that started before the swap has finished. The wait is
/// bounded by `budget`; dispatches themselves are bounded by the
/// per-plugin timeouts, so the steady-state wait is at most one
/// frame's dispatch latency.
///
/// `budget` is shared across the quiesce and drain phases (same
/// philosophy as `drain_all`'s shared per-plugin deadline). A zero
/// budget skips both phases — the documented
/// `reload_drain_timeout_ms = 0` opt-out (legacy swap → shutdown
/// behaviour; in-flight work may be lost).
pub async fn swap_and_retire(
    shared: &SharedRegistry,
    new_registry: PluginRegistry,
    budget: Duration,
) -> RetireReport {
    let old = shared.swap(Arc::new(new_registry));

    if budget.is_zero() {
        old.shutdown_all();
        return RetireReport {
            quiesced_cleanly: true,
            drain_results: Vec::new(),
            retire_duration: Duration::ZERO,
        };
    }

    let started = Instant::now();
    let deadline = started + budget;

    let quiesced_cleanly = wait_for_snapshot_release(&old, deadline).await;
    if !quiesced_cleanly {
        tracing::error!(
            budget_ms = budget.as_millis(),
            "in-flight dispatches still hold the previous registry snapshot at the \
             drain deadline; proceeding with drain + shutdown (their fire-and-forget \
             follow-up work may be lost)"
        );
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    let drain_results = old.drain_all(remaining);

    old.shutdown_all();

    let retire_duration = started.elapsed();
    let forced = drain_results.iter().filter(|(_, ok)| !ok).count();
    tracing::info!(
        retire_ms = retire_duration.as_millis(),
        quiesced_cleanly,
        drained = drain_results.len() - forced,
        forced,
        budget_ms = budget.as_millis(),
        "previous registry retired"
    );

    RetireReport {
        quiesced_cleanly,
        drain_results,
        retire_duration,
    }
}

/// Wait until `old`'s strong count drops to 1 (only the caller's
/// handle remains — no in-flight dispatch holds a snapshot anymore)
/// or `deadline` passes. Returns whether the count reached 1.
async fn wait_for_snapshot_release(old: &Arc<PluginRegistry>, deadline: Instant) -> bool {
    loop {
        if Arc::strong_count(old) == 1 {
            return true;
        }
        if Instant::now() >= deadline {
            // One final check: a dispatch may have released its
            // snapshot concurrently with the deadline expiring.
            return Arc::strong_count(old) == 1;
        }
        tokio::time::sleep(QUIESCE_POLL_INTERVAL).await;
    }
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

    #[test]
    fn queue_reservations_never_exceed_the_configured_cap() {
        let admission = Arc::new(PluginAdmission::new(1, 2, None, None));
        let contenders = (0..32)
            .map(|_| {
                let admission = Arc::clone(&admission);
                std::thread::spawn(move || admission.try_enter_queue())
            })
            .collect::<Vec<_>>();

        let admitted = contenders
            .into_iter()
            .map(|thread| thread.join().expect("queue contender should not panic"))
            .filter(|admitted| *admitted)
            .count();

        assert_eq!(admitted, 2);
        admission.leave_queue();
        assert!(admission.try_enter_queue());
    }

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

    // --- swap_and_retire budget semantics ---------------------------
    //
    // The full in-flight-dispatch choreography (quiesce blocks until
    // the last snapshot is released; fire-and-forget work survives the
    // reload) lives in crates/hub/tests/test_reload_quiesce.rs. The
    // tests here pin the budget edge cases against a real plugin.

    use crate::test_support::load_noop_plugin;

    #[tokio::test]
    async fn swap_and_retire_zero_budget_skips_quiesce_and_drain() {
        let shared = shared_registry(vec![load_noop_plugin()]).unwrap();
        // Hold a snapshot: the documented `reload_drain_timeout_ms = 0`
        // opt-out must NOT wait for it (legacy swap → shutdown).
        let _snapshot = shared.load_full();
        let report = swap_and_retire(
            &shared,
            PluginRegistry::build(vec![]).unwrap(),
            Duration::ZERO,
        )
        .await;
        assert!(report.quiesced_cleanly);
        assert!(report.drain_results.is_empty());
        assert_eq!(report.retire_duration, Duration::ZERO);
    }

    #[tokio::test]
    async fn swap_and_retire_quiescent_registry_completes_promptly() {
        let shared = shared_registry(vec![load_noop_plugin()]).unwrap();
        let started = Instant::now();
        let report = swap_and_retire(
            &shared,
            PluginRegistry::build(vec![]).unwrap(),
            Duration::from_secs(5),
        )
        .await;
        assert!(report.quiesced_cleanly);
        assert_eq!(report.drain_results.len(), 1);
        assert!(report.drain_results[0].1, "noop drains clean");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "no snapshots held — retire must not burn the budget, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn swap_and_retire_reports_forced_quiesce_on_deadline() {
        let shared = shared_registry(vec![load_noop_plugin()]).unwrap();
        // A snapshot held past the whole budget: retire must give up
        // at the deadline (bounded reload latency) and report it.
        let snapshot = shared.load_full();
        let report = swap_and_retire(
            &shared,
            PluginRegistry::build(vec![]).unwrap(),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            !report.quiesced_cleanly,
            "a still-held snapshot at the deadline must be reported as forced"
        );
        drop(snapshot);
    }
}
