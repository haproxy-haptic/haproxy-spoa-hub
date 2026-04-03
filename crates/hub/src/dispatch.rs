use std::sync::Arc;
use std::time::Instant;

use haproxy_spoa_hub_plugin_api::{ProcessingResult, SpoeMessage, SpoeValue, VarScope};
use spop::actions::VarScope as SpopVarScope;
use spop::types::TypedData;

use crate::registry::{ExecutionPlan, PluginRegistry};

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
pub fn var_scope_to_spop(scope: &VarScope) -> SpopVarScope {
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
#[must_use]
pub fn spoe_value_to_typed_data(val: &SpoeValue) -> TypedData {
    match val {
        SpoeValue::Null => TypedData::Null,
        SpoeValue::Bool(b) => TypedData::Bool(*b),
        SpoeValue::Int32(v) => TypedData::Int32(*v),
        SpoeValue::Uint32(v) => TypedData::UInt32(*v),
        SpoeValue::Int64(v) => TypedData::Int64(*v),
        SpoeValue::Uint64(v) => TypedData::UInt64(*v),
        SpoeValue::Ipv4(octets) => TypedData::IPv4(std::net::Ipv4Addr::from(*octets)),
        SpoeValue::Ipv6(octets) => TypedData::IPv6(std::net::Ipv6Addr::from(*octets)),
        SpoeValue::String(s) => TypedData::String(s.to_string()),
        SpoeValue::Binary(b) => TypedData::Binary(b.to_vec()),
        // Unknown value type — convert to Null to avoid data loss in ACK
        #[allow(clippy::match_same_arms)]
        SpoeValue::__Other(_, _) => TypedData::Null,
    }
}

/// Result of dispatching a message to all plugins.
pub struct DispatchResult {
    /// All collected transaction variables with namespace prefixes applied.
    pub variables: Vec<(VarScope, String, SpoeValue)>,
}

/// Dispatch a message through the execution plan, running independent
/// plugins in parallel and respecting dependency ordering.
pub async fn dispatch_message(
    message: &SpoeMessage,
    plan: &ExecutionPlan,
    registry: &Arc<PluginRegistry>,
) -> DispatchResult {
    let mut all_variables: Vec<(VarScope, String, SpoeValue)> = Vec::new();
    // Track whether upstream plugins have added variables.
    // Only clone args when needed for dependency data flow.
    let mut extra_args: Option<
        abi_stable::std_types::RHashMap<abi_stable::std_types::RString, SpoeValue>,
    > = None;

    for group in &plan.groups {
        let group_results = execute_group(group, message, extra_args.as_ref(), registry).await;

        for (plugin_name, result) in group_results {
            for var in &result.variables {
                let prefixed_name = format!("{}.{}", plugin_name, var.name);
                all_variables.push((var.scope, prefixed_name.clone(), var.value.clone()));
                // Lazily create extra_args only when there are downstream groups
                extra_args
                    .get_or_insert_with(abi_stable::std_types::RHashMap::new)
                    .insert(prefixed_name.into(), var.value.clone());
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
    group: &[String],
    message: &SpoeMessage,
    extra_args: Option<&abi_stable::std_types::RHashMap<abi_stable::std_types::RString, SpoeValue>>,
    registry: &Arc<PluginRegistry>,
) -> Vec<(String, ProcessingResult)> {
    if group.len() == 1 {
        // Fast path: single plugin — one spawn_blocking, no tokio::spawn,
        // no join_all. Timeout still enforced via spawn_blocking yield.
        return call_single_plugin(&group[0], message, extra_args, registry)
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

    for plugin_name in group {
        let Some(entry) = registry.get_plugin(plugin_name) else {
            tracing::error!(plugin = %plugin_name, "plugin not found in registry");
            continue;
        };

        let instance = Arc::clone(&entry.instance);
        let name = plugin_name.clone();
        let msg = build_message(message, extra_args);
        let timeout = entry.timeout;

        // Launch on blocking thread pool immediately
        let handle = tokio::task::spawn_blocking(move || instance.process(&msg).into_result());

        // Create a future that wraps the handle with a timeout
        futures.push(async move {
            let start = Instant::now();
            let result = tokio::time::timeout(timeout, handle).await;
            let elapsed = start.elapsed();

            match result {
                Ok(Ok(Ok(processing_result))) => {
                    tracing::debug!(
                        plugin = %name,
                        latency_us = elapsed.as_micros(),
                        variables = processing_result.variables.len(),
                        "plugin completed"
                    );
                    Some((name, processing_result))
                }
                Ok(Ok(Err(e))) => {
                    tracing::error!(
                        plugin = %name,
                        error = %e,
                        latency_us = elapsed.as_micros(),
                        "plugin returned error"
                    );
                    crate::metrics::record_plugin_error(&name);
                    None
                }
                Ok(Err(join_err)) => {
                    tracing::error!(
                        plugin = %name,
                        error = %join_err,
                        "plugin task panicked"
                    );
                    crate::metrics::record_plugin_error(&name);
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        plugin = %name,
                        timeout_ms = %timeout.as_millis(),
                        "plugin timed out"
                    );
                    crate::metrics::record_plugin_timeout(&name);
                    None
                }
            }
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
    extra_args: Option<&abi_stable::std_types::RHashMap<abi_stable::std_types::RString, SpoeValue>>,
    registry: &Arc<PluginRegistry>,
) -> Option<(String, ProcessingResult)> {
    let entry = registry.get_plugin(plugin_name)?;
    let instance = Arc::clone(&entry.instance);
    let timeout = entry.timeout;
    let name = plugin_name.to_string();

    // Build message: avoid clone if no extra args
    let msg = if extra_args.is_some() {
        build_message(message, extra_args)
    } else {
        message.clone()
    };

    let start = Instant::now();
    let handle = tokio::task::spawn_blocking(move || instance.process(&msg).into_result());

    match tokio::time::timeout(timeout, handle).await {
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
            crate::metrics::record_plugin_error(&name);
            None
        }
        Ok(Err(join_err)) => {
            tracing::error!(
                plugin = %name,
                error = %join_err,
                "plugin task panicked"
            );
            crate::metrics::record_plugin_error(&name);
            None
        }
        Err(_) => {
            tracing::warn!(
                plugin = %name,
                timeout_ms = %timeout.as_millis(),
                "plugin timed out"
            );
            crate::metrics::record_plugin_timeout(&name);
            None
        }
    }
}

/// Build a message with extra args merged in (for dependency data flow).
fn build_message(
    original: &SpoeMessage,
    extra_args: Option<&abi_stable::std_types::RHashMap<abi_stable::std_types::RString, SpoeValue>>,
) -> SpoeMessage {
    let mut msg = original.clone();
    if let Some(extras) = extra_args {
        for pair in extras {
            msg.args.insert(pair.0.clone(), pair.1.clone());
        }
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
        if let SpoeValue::String(s) = result {
            assert_eq!(s.as_str(), "hello");
        } else {
            panic!("expected String variant");
        }
    }

    #[test]
    fn test_typed_data_ipv4_conversion() {
        let addr = std::net::Ipv4Addr::new(127, 0, 0, 1);
        let result = typed_data_to_spoe_value(&TypedData::IPv4(addr));
        assert!(matches!(result, SpoeValue::Ipv4([127, 0, 0, 1])));
    }

    #[test]
    fn test_typed_data_ipv6_conversion() {
        let addr = std::net::Ipv6Addr::LOCALHOST;
        let result = typed_data_to_spoe_value(&TypedData::IPv6(addr));
        if let SpoeValue::Ipv6(octets) = result {
            assert_eq!(octets, addr.octets());
        } else {
            panic!("expected Ipv6 variant");
        }
    }

    #[test]
    fn test_typed_data_binary_conversion() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = typed_data_to_spoe_value(&TypedData::Binary(data.clone()));
        if let SpoeValue::Binary(b) = result {
            assert_eq!(b.as_slice(), &data);
        } else {
            panic!("expected Binary variant");
        }
    }

    #[test]
    fn test_var_scope_roundtrip() {
        assert!(matches!(
            var_scope_to_spop(&VarScope::Process),
            SpopVarScope::Process
        ));
        assert!(matches!(
            var_scope_to_spop(&VarScope::Session),
            SpopVarScope::Session
        ));
        assert!(matches!(
            var_scope_to_spop(&VarScope::Transaction),
            SpopVarScope::Transaction
        ));
        assert!(matches!(
            var_scope_to_spop(&VarScope::Request),
            SpopVarScope::Request
        ));
        assert!(matches!(
            var_scope_to_spop(&VarScope::Response),
            SpopVarScope::Response
        ));
    }

    #[test]
    fn test_spoe_value_to_typed_data_roundtrip() {
        // String roundtrip
        let original = TypedData::String("test".to_string());
        let spoe = typed_data_to_spoe_value(&original);
        let back = spoe_value_to_typed_data(&spoe);
        assert!(matches!(back, TypedData::String(s) if s == "test"));

        // UInt32 roundtrip
        let original = TypedData::UInt32(42);
        let spoe = typed_data_to_spoe_value(&original);
        let back = spoe_value_to_typed_data(&spoe);
        assert!(matches!(back, TypedData::UInt32(42)));

        // IPv4 roundtrip
        let addr = std::net::Ipv4Addr::new(192, 168, 1, 1);
        let original = TypedData::IPv4(addr);
        let spoe = typed_data_to_spoe_value(&original);
        let back = spoe_value_to_typed_data(&spoe);
        assert!(matches!(back, TypedData::IPv4(a) if a == addr));
    }

    // The following tests exercise the async dispatch logic with real
    // plugin instances. They require the noop plugin .so to be built.

    fn workspace_target_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("debug")
    }

    fn load_noop_plugin() -> crate::plugin_loader::LoadedPlugin {
        let dir = workspace_target_dir();
        let config = crate::config::PluginConfig {
            name: "noop".into(),
            library: "libnoop_plugin.so".into(),
            messages: vec!["test-msg".into()],
            depends_on: vec![],
            timeout_ms: Some(1000),
            params: Default::default(),
        };
        crate::plugin_loader::load_plugin(&dir, &config, 100).unwrap()
    }

    #[tokio::test]
    async fn test_dispatch_single_plugin_returns_prefixed_vars() {
        let plugin = load_noop_plugin();
        let registry = Arc::new(crate::registry::PluginRegistry::build(vec![plugin]).unwrap());
        let plan = registry.plan_for_message("test-msg").unwrap().clone();

        let msg = SpoeMessage {
            name: "test-msg".into(),
            args: abi_stable::std_types::RHashMap::new(),
            stream_id: 1,
            frame_id: 1,
        };

        let result = dispatch_message(&msg, &plan, &registry).await;
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
        let registry = Arc::new(crate::registry::PluginRegistry::build(vec![plugin]).unwrap());

        // No plan for "nonexistent" message
        assert!(registry.plan_for_message("nonexistent").is_none());
    }
}
