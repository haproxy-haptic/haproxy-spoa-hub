use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use abi_stable::std_types::{RHashMap, RString, Tuple2};
use haproxy_spoa_hub_plugin_api::{ProcessingResult, SpoeMessage, SpoeValue, VarScope};
use spop::{actions::VarScope as SpopVarScope, types::TypedData};

use crate::{
    metrics,
    registry::{ExecutionPlan, PluginRegistry},
};

/// Result of `tokio::time::timeout(spawn_blocking(plugin.process()))`.
///
/// The three nesting layers reflect the runtime pipeline:
/// - Outer: `timeout` → `Elapsed` on expiry
/// - Middle: `spawn_blocking` → `JoinError` on panic/cancellation
/// - Inner: plugin `process()` → `RBoxError` on plugin failure
type PluginCallResult = Result<
    Result<Result<ProcessingResult, abi_stable::std_types::RBoxError>, tokio::task::JoinError>,
    tokio::time::error::Elapsed,
>;

/// Alias for the extra-args map threaded through the dispatch pipeline.
type ArgMap = RHashMap<RString, SpoeValue>;

/// Prefix for hub control args. Args with this prefix are consumed by
/// the hub and stripped from the message before plugins see them.
const HUB_ARG_PREFIX: &str = "hub.";

/// Convert a spop `TypedData` value to our FFI-safe `SpoeValue`.
#[must_use]
pub fn typed_data_to_spoe_value(td: &TypedData) -> SpoeValue {
    match td {
        TypedData::Null => SpoeValue::Null,
        TypedData::Bool(b) => SpoeValue::Bool(*b),
        TypedData::Int32(v) => SpoeValue::Int32(*v),
        TypedData::UInt32(v) => SpoeValue::Uint32(*v),
        TypedData::Int64(v) => SpoeValue::Int64(*v),
        TypedData::UInt64(v) => SpoeValue::Uint64(*v),
        TypedData::IPv4(addr) => SpoeValue::Ipv4(addr.octets()),
        TypedData::IPv6(addr) => SpoeValue::Ipv6(addr.octets()),
        TypedData::String(s) => SpoeValue::String(s.as_str().into()),
        TypedData::Binary(b) => SpoeValue::Binary(b.as_slice().into()),
    }
}

/// Convert our `VarScope` to spop's `VarScope`.
#[must_use]
pub fn var_scope_to_spop(scope: VarScope) -> SpopVarScope {
    match scope {
        VarScope::Process => SpopVarScope::Process,
        VarScope::Session => SpopVarScope::Session,
        VarScope::Transaction => SpopVarScope::Transaction,
        VarScope::Request => SpopVarScope::Request,
        VarScope::Response => SpopVarScope::Response,
        // Unknown scope — default to Transaction as the safest option
        #[allow(clippy::match_same_arms)]
        VarScope::__Other(_) => SpopVarScope::Transaction,
    }
}

/// Convert a `SpoeValue` to a spop `TypedData` for ACK building.
///
/// Consumes the value to move String and Binary contents without cloning.
#[must_use]
pub fn spoe_value_into_typed_data(val: SpoeValue) -> TypedData {
    match val {
        SpoeValue::Null => TypedData::Null,
        SpoeValue::Bool(b) => TypedData::Bool(b),
        SpoeValue::Int32(v) => TypedData::Int32(v),
        SpoeValue::Uint32(v) => TypedData::UInt32(v),
        SpoeValue::Int64(v) => TypedData::Int64(v),
        SpoeValue::Uint64(v) => TypedData::UInt64(v),
        SpoeValue::Ipv4(octets) => TypedData::IPv4(Ipv4Addr::from(octets)),
        SpoeValue::Ipv6(octets) => TypedData::IPv6(Ipv6Addr::from(octets)),
        SpoeValue::String(s) => TypedData::String(s.into_string()),
        SpoeValue::Binary(b) => TypedData::Binary(b.into_vec()),
        // Unknown value type — convert to Null to avoid data loss in ACK
        #[allow(clippy::match_same_arms)]
        SpoeValue::__Other(_, _) => TypedData::Null,
    }
}

/// Extract hub control args from a message, returning the skip set and
/// a cleaned message with all `hub.*` args removed.
///
/// Hub control args use the `hub.` prefix and are consumed by the hub
/// dispatch layer. Plugins never see them.
fn extract_hub_args(message: &SpoeMessage) -> (HashSet<String>, Option<SpoeMessage>) {
    let has_hub_args = message
        .args
        .iter()
        .any(|Tuple2(key, _)| key.starts_with(HUB_ARG_PREFIX));

    if !has_hub_args {
        return (HashSet::new(), None);
    }

    let mut skip_set = HashSet::new();
    let mut cleaned_args = RHashMap::new();

    for Tuple2(key, value) in &message.args {
        let Some(suffix) = key.strip_prefix(HUB_ARG_PREFIX) else {
            cleaned_args.insert(key.clone(), value.clone());
            continue;
        };

        // All hub.* args consumed regardless of whether they're recognized
        if suffix == "skip_plugins" {
            if let SpoeValue::String(s) = value {
                skip_set = s
                    .as_str()
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
    }

    let cleaned = SpoeMessage {
        name: message.name.clone(),
        args: cleaned_args,
        stream_id: message.stream_id,
        frame_id: message.frame_id,
    };

    (skip_set, Some(cleaned))
}

/// Expand the skip set by adding plugins that depend on any skipped plugin.
///
/// Called after processing each execution group. Because groups are
/// topologically sorted, upstream skip decisions are finalized before
/// downstream groups are evaluated.
fn expand_skip_cascade(
    skipped_name: &str,
    registry: &PluginRegistry,
    skip_set: &mut HashSet<String>,
) {
    for name in registry.dependents_of(skipped_name) {
        if skip_set.insert(name.clone()) {
            tracing::debug!(
                plugin = %name,
                reason = %skipped_name,
                "plugin skipped (dependency cascade)"
            );
            metrics::record_plugin_skip(name);
        }
    }
}

/// Result of dispatching a message to all plugins.
pub struct DispatchResult {
    /// All collected transaction variables with namespace prefixes applied.
    pub variables: Vec<(VarScope, String, SpoeValue)>,
}

/// Dispatch a message through the execution plan, running independent
/// plugins in parallel and respecting dependency ordering.
///
/// Hub control args (`hub.*` prefix) are extracted and stripped before
/// any plugin sees the message. The `hub.skip_plugins` arg skips named
/// plugins and their transitive dependents.
pub async fn dispatch_message(
    message: &SpoeMessage,
    plan: &ExecutionPlan,
    registry: &Arc<PluginRegistry>,
) -> DispatchResult {
    let (mut skip_set, cleaned_message) = extract_hub_args(message);
    let effective_message = cleaned_message.as_ref().unwrap_or(message);

    let mut all_variables = Vec::new();
    let mut extra_args: Option<ArgMap> = None;

    for group in &plan.groups {
        // Filter out skipped plugins and cascade to dependents in a single pass.
        // Cascade is safe here: plugins in the same group are independent
        // (topologically), so expanding the skip set cannot affect this group.
        let mut active: Vec<&str> = Vec::with_capacity(group.len());
        for name in group {
            if skip_set.contains(name) {
                tracing::debug!(plugin = %name, "plugin skipped");
                metrics::record_plugin_skip(name);
                expand_skip_cascade(name, registry, &mut skip_set);
            } else {
                active.push(name);
            }
        }

        if active.is_empty() {
            continue;
        }

        let group_results =
            execute_group(&active, effective_message, extra_args.as_ref(), registry).await;

        for (plugin_name, result) in group_results {
            for var in result.variables {
                let prefixed_name = format!("{}.{}", plugin_name, var.name);
                extra_args
                    .get_or_insert_with(RHashMap::new)
                    .insert(prefixed_name.as_str().into(), var.value.clone());
                all_variables.push((var.scope, prefixed_name, var.value));
            }
        }
    }

    DispatchResult {
        variables: all_variables,
    }
}

/// Execute a group of plugins. Uses the fastest path available:
/// - Single plugin: `spawn_blocking` + timeout (no `tokio::spawn`)
/// - Multiple plugins: `spawn_blocking` each, `join_all` with timeouts
async fn execute_group(
    group: &[&str],
    message: &SpoeMessage,
    extra_args: Option<&ArgMap>,
    registry: &Arc<PluginRegistry>,
) -> Vec<(String, ProcessingResult)> {
    if group.len() == 1 {
        // Fast path: single plugin — one spawn_blocking, no tokio::spawn,
        // no join_all. Timeout still enforced via spawn_blocking yield.
        return call_single_plugin(group[0], message, extra_args, registry)
            .await
            .into_iter()
            .collect();
    }

    // Parallel path: multiple plugins in this group.
    //
    // All spawn_blocking calls are launched up front so plugins execute
    // concurrently on the blocking thread pool. We then await all of
    // them concurrently using join_all with per-plugin timeouts.
    //
    // spawn_blocking is needed (rather than a direct call) because:
    // 1. Plugins may block (sleep, I/O) which would prevent timeout from firing
    // 2. True thread-level parallelism for CPU-bound plugins
    let mut futures = Vec::with_capacity(group.len());

    for &plugin_name in group {
        let Some(entry) = registry.get_plugin(plugin_name) else {
            tracing::error!(plugin = %plugin_name, "plugin not found in registry");
            continue;
        };

        let instance = Arc::clone(&entry.instance);
        let name = plugin_name.to_owned();
        let msg = build_message(message, extra_args);
        let timeout = entry.timeout;

        // Launch on blocking thread pool immediately
        let handle = tokio::task::spawn_blocking(move || instance.process(&msg).into_result());

        // Create a future that wraps the handle with a timeout
        futures.push(async move {
            let start = Instant::now();
            let result = tokio::time::timeout(timeout, handle).await;
            handle_plugin_result(name, result, start, timeout)
        });
    }

    // Await all plugin futures concurrently
    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Single-plugin fast path: one `spawn_blocking` call with timeout.
/// Avoids `tokio::spawn` and `join_all` overhead compared to the parallel path.
async fn call_single_plugin(
    plugin_name: &str,
    message: &SpoeMessage,
    extra_args: Option<&ArgMap>,
    registry: &Arc<PluginRegistry>,
) -> Option<(String, ProcessingResult)> {
    let entry = registry.get_plugin(plugin_name)?;
    let instance = Arc::clone(&entry.instance);
    let timeout = entry.timeout;
    let name = plugin_name.to_owned();

    let msg = build_message(message, extra_args);

    let start = Instant::now();
    let handle = tokio::task::spawn_blocking(move || instance.process(&msg).into_result());
    let result = tokio::time::timeout(timeout, handle).await;
    handle_plugin_result(name, result, start, timeout)
}

/// Classify and log the outcome of a single plugin `spawn_blocking` + timeout call.
fn handle_plugin_result(
    name: String,
    result: PluginCallResult,
    start: Instant,
    timeout: Duration,
) -> Option<(String, ProcessingResult)> {
    match result {
        Ok(Ok(Ok(processing_result))) => {
            tracing::debug!(
                plugin = %name,
                latency_us = start.elapsed().as_micros(),
                variables = processing_result.variables.len(),
                "plugin completed"
            );
            Some((name, processing_result))
        }
        Ok(Ok(Err(e))) => {
            tracing::error!(
                plugin = %name,
                error = %e,
                latency_us = start.elapsed().as_micros(),
                "plugin returned error"
            );
            metrics::record_plugin_error(&name);
            None
        }
        Ok(Err(join_err)) => {
            tracing::error!(
                plugin = %name,
                error = %join_err,
                "plugin task panicked"
            );
            metrics::record_plugin_error(&name);
            None
        }
        Err(_) => {
            tracing::warn!(
                plugin = %name,
                timeout_ms = %timeout.as_millis(),
                "plugin timed out"
            );
            metrics::record_plugin_timeout(&name);
            None
        }
    }
}

/// Build a message with extra args merged in (for dependency data flow).
fn build_message(original: &SpoeMessage, extra_args: Option<&ArgMap>) -> SpoeMessage {
    let Some(extras) = extra_args else {
        return original.clone();
    };
    let mut msg = original.clone();
    for Tuple2(key, value) in extras {
        msg.args.insert(key.clone(), value.clone());
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typed_data_null_conversion() {
        let result = typed_data_to_spoe_value(&TypedData::Null);
        assert!(matches!(result, SpoeValue::Null));
    }

    #[test]
    fn test_typed_data_bool_conversion() {
        let result = typed_data_to_spoe_value(&TypedData::Bool(true));
        assert!(matches!(result, SpoeValue::Bool(true)));
    }

    #[test]
    fn test_typed_data_int32_conversion() {
        let result = typed_data_to_spoe_value(&TypedData::Int32(-42));
        assert!(matches!(result, SpoeValue::Int32(-42)));
    }

    #[test]
    fn test_typed_data_string_conversion() {
        let result = typed_data_to_spoe_value(&TypedData::String("hello".to_string()));
        let SpoeValue::String(s) = result else {
            panic!("expected String variant");
        };
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn test_typed_data_ipv4_conversion() {
        let addr = Ipv4Addr::LOCALHOST;
        let result = typed_data_to_spoe_value(&TypedData::IPv4(addr));
        assert!(matches!(result, SpoeValue::Ipv4([127, 0, 0, 1])));
    }

    #[test]
    fn test_typed_data_ipv6_conversion() {
        let addr = Ipv6Addr::LOCALHOST;
        let result = typed_data_to_spoe_value(&TypedData::IPv6(addr));
        let SpoeValue::Ipv6(octets) = result else {
            panic!("expected Ipv6 variant");
        };
        assert_eq!(octets, addr.octets());
    }

    #[test]
    fn test_typed_data_binary_conversion() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = typed_data_to_spoe_value(&TypedData::Binary(data.clone()));
        let SpoeValue::Binary(b) = result else {
            panic!("expected Binary variant");
        };
        assert_eq!(b.as_slice(), &data);
    }

    #[test]
    fn test_var_scope_roundtrip() {
        assert!(matches!(
            var_scope_to_spop(VarScope::Process),
            SpopVarScope::Process
        ));
        assert!(matches!(
            var_scope_to_spop(VarScope::Session),
            SpopVarScope::Session
        ));
        assert!(matches!(
            var_scope_to_spop(VarScope::Transaction),
            SpopVarScope::Transaction
        ));
        assert!(matches!(
            var_scope_to_spop(VarScope::Request),
            SpopVarScope::Request
        ));
        assert!(matches!(
            var_scope_to_spop(VarScope::Response),
            SpopVarScope::Response
        ));
    }

    #[test]
    fn test_spoe_value_to_typed_data_roundtrip() {
        // String roundtrip
        let original = TypedData::String("test".to_string());
        let spoe = typed_data_to_spoe_value(&original);
        let back = spoe_value_into_typed_data(spoe);
        assert!(matches!(back, TypedData::String(s) if s == "test"));

        // UInt32 roundtrip
        let original = TypedData::UInt32(42);
        let spoe = typed_data_to_spoe_value(&original);
        let back = spoe_value_into_typed_data(spoe);
        assert!(matches!(back, TypedData::UInt32(42)));

        // IPv4 roundtrip
        let addr = Ipv4Addr::new(192, 168, 1, 1);
        let original = TypedData::IPv4(addr);
        let spoe = typed_data_to_spoe_value(&original);
        let back = spoe_value_into_typed_data(spoe);
        assert!(matches!(back, TypedData::IPv4(a) if a == addr));
    }

    // The following tests exercise the async dispatch logic with real
    // plugin instances. They require the noop plugin .so to be built.

    use crate::plugin_loader;
    use crate::test_support::load_noop_plugin;

    #[tokio::test]
    async fn test_dispatch_single_plugin_returns_prefixed_vars() {
        let plugin = load_noop_plugin();
        let registry = Arc::new(PluginRegistry::build(vec![plugin]).unwrap());
        let plan = registry.plan_for_message("test-msg").unwrap();

        let msg = SpoeMessage {
            name: "test-msg".into(),
            args: RHashMap::new(),
            stream_id: 1,
            frame_id: 1,
        };

        let result = dispatch_message(&msg, plan, &registry).await;
        assert!(
            !result.variables.is_empty(),
            "expected at least one variable"
        );

        // Verify namespace prefix
        let (_, name, _) = &result.variables[0];
        assert!(
            name.starts_with("noop."),
            "expected noop. prefix, got: {name}"
        );
    }

    #[tokio::test]
    async fn test_dispatch_no_plan_returns_empty() {
        let plugin = load_noop_plugin();
        let registry = Arc::new(PluginRegistry::build(vec![plugin]).unwrap());

        // No plan for "nonexistent" message
        assert!(registry.plan_for_message("nonexistent").is_none());
    }

    // --- Hub control arg tests ---

    fn make_message_with_args(args: Vec<(&str, SpoeValue)>) -> SpoeMessage {
        let mut rargs = RHashMap::new();
        for (k, v) in args {
            rargs.insert(RString::from(k), v);
        }
        SpoeMessage {
            name: "test".into(),
            args: rargs,
            stream_id: 1,
            frame_id: 1,
        }
    }

    #[test]
    fn test_extract_hub_args_parses_skip_plugins() {
        let msg = make_message_with_args(vec![(
            "hub.skip_plugins",
            SpoeValue::String("a,b,c".into()),
        )]);
        let (skip_set, _) = extract_hub_args(&msg);
        assert_eq!(
            skip_set,
            HashSet::from(["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn test_extract_hub_args_trims_whitespace() {
        let msg = make_message_with_args(vec![(
            "hub.skip_plugins",
            SpoeValue::String("a , b ".into()),
        )]);
        let (skip_set, _) = extract_hub_args(&msg);
        assert_eq!(skip_set, HashSet::from(["a".into(), "b".into()]));
    }

    #[test]
    fn test_extract_hub_args_empty_when_missing() {
        let msg = make_message_with_args(vec![("src", SpoeValue::String("1.2.3.4".into()))]);
        let (skip_set, cleaned) = extract_hub_args(&msg);
        assert!(skip_set.is_empty());
        assert!(cleaned.is_none(), "no hub args means no cleaned message");
    }

    #[test]
    fn test_extract_hub_args_ignores_non_string() {
        let msg = make_message_with_args(vec![("hub.skip_plugins", SpoeValue::Bool(true))]);
        let (skip_set, _) = extract_hub_args(&msg);
        assert!(skip_set.is_empty());
    }

    #[test]
    fn test_extract_hub_args_strips_hub_prefix() {
        let msg = make_message_with_args(vec![
            ("src", SpoeValue::String("1.2.3.4".into())),
            ("hub.skip_plugins", SpoeValue::String("noop".into())),
            ("hub.future_arg", SpoeValue::Int32(42)),
            ("method", SpoeValue::String("GET".into())),
        ]);
        let (_, cleaned) = extract_hub_args(&msg);
        let cleaned = cleaned.expect("hub args present, should return cleaned message");
        // Regular args kept
        assert!(cleaned.args.get(&RString::from("src")).is_some());
        assert!(cleaned.args.get(&RString::from("method")).is_some());
        // Hub args stripped
        assert!(
            cleaned
                .args
                .get(&RString::from("hub.skip_plugins"))
                .is_none()
        );
        assert!(cleaned.args.get(&RString::from("hub.future_arg")).is_none());
        assert_eq!(cleaned.args.len(), 2);
    }

    fn test_plugin(name: &str, depends_on: &[&str]) -> plugin_loader::LoadedPlugin {
        plugin_loader::LoadedPlugin {
            name: name.into(),
            version: "0.1.0".into(),
            instance: load_noop_plugin().instance,
            messages: vec!["msg".into()],
            depends_on: depends_on.iter().map(|&d| d.into()).collect(),
            timeout: Duration::from_millis(100),
        }
    }

    #[test]
    fn test_expand_skip_cascade_skips_direct_dependent() {
        let plugins = vec![test_plugin("a", &[]), test_plugin("b", &["a"])];
        let registry = PluginRegistry::build(plugins).unwrap();
        let mut skip_set = HashSet::from(["a".to_string()]);
        expand_skip_cascade("a", &registry, &mut skip_set);
        assert!(skip_set.contains("b"), "b should be cascaded from a");
    }

    #[test]
    fn test_expand_skip_cascade_preserves_independent() {
        let plugins = vec![test_plugin("a", &[]), test_plugin("c", &[])];
        let registry = PluginRegistry::build(plugins).unwrap();
        let mut skip_set = HashSet::from(["a".to_string()]);
        expand_skip_cascade("a", &registry, &mut skip_set);
        assert!(
            !skip_set.contains("c"),
            "c should NOT be cascaded (independent)"
        );
    }

    #[tokio::test]
    async fn test_dispatch_skips_named_plugin() {
        let plugin = load_noop_plugin();
        let registry = Arc::new(PluginRegistry::build(vec![plugin]).unwrap());
        let plan = registry.plan_for_message("test-msg").unwrap();

        let msg =
            make_message_with_args(vec![("hub.skip_plugins", SpoeValue::String("noop".into()))]);

        let result = dispatch_message(&msg, plan, &registry).await;
        assert!(
            result.variables.is_empty(),
            "expected no variables when plugin is skipped, got: {:?}",
            result.variables.len()
        );
    }
}
