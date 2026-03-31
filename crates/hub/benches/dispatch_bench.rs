use std::path::PathBuf;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use haproxy_spoa_hub::dispatch::dispatch_message;
use haproxy_spoa_hub::registry::PluginRegistry;
use haproxy_spoa_hub_plugin_api::SpoeMessage;

fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
}

fn build_single_noop_registry() -> Arc<PluginRegistry> {
    let dir = workspace_target_dir();
    let config = haproxy_spoa_hub::config::PluginConfig {
        name: "noop".into(),
        library: "libnoop_plugin.so".into(),
        messages: vec!["bench-msg".into()],
        depends_on: vec![],
        timeout_ms: Some(5000),
        params: Default::default(),
    };
    let loaded = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config, 5000).unwrap();
    Arc::new(PluginRegistry::build(vec![loaded]).unwrap())
}

fn build_two_plugin_registry() -> Arc<PluginRegistry> {
    let dir = workspace_target_dir();
    let config_a = haproxy_spoa_hub::config::PluginConfig {
        name: "noop-a".into(),
        library: "libnoop_plugin.so".into(),
        messages: vec!["bench-parallel".into()],
        depends_on: vec![],
        timeout_ms: Some(5000),
        params: Default::default(),
    };
    let config_b = haproxy_spoa_hub::config::PluginConfig {
        name: "noop-b".into(),
        library: "libnoop_plugin.so".into(),
        messages: vec!["bench-parallel".into()],
        depends_on: vec![],
        timeout_ms: Some(5000),
        params: Default::default(),
    };
    let a = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config_a, 5000).unwrap();
    let b = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config_b, 5000).unwrap();
    Arc::new(PluginRegistry::build(vec![a, b]).unwrap())
}

fn bench_dispatch_single_plugin(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = build_single_noop_registry();
    let msg = SpoeMessage {
        name: "bench-msg".into(),
        args: abi_stable::std_types::RHashMap::new(),
        stream_id: 1,
        frame_id: 1,
    };
    let plan = registry.plan_for_message("bench-msg").unwrap().clone();

    c.bench_function("dispatch_1_plugin", |b| {
        b.iter(|| rt.block_on(dispatch_message(&msg, &plan, &registry)));
    });
}

fn bench_dispatch_two_plugins_parallel(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = build_two_plugin_registry();
    let msg = SpoeMessage {
        name: "bench-parallel".into(),
        args: abi_stable::std_types::RHashMap::new(),
        stream_id: 1,
        frame_id: 1,
    };
    let plan = registry.plan_for_message("bench-parallel").unwrap().clone();

    c.bench_function("dispatch_2_plugins_parallel", |b| {
        b.iter(|| rt.block_on(dispatch_message(&msg, &plan, &registry)));
    });
}

fn bench_dispatch_with_args(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = build_single_noop_registry();
    let mut args = abi_stable::std_types::RHashMap::new();
    args.insert(
        "src".into(),
        haproxy_spoa_hub_plugin_api::SpoeValue::String("10.0.0.1".into()),
    );
    args.insert(
        "dst".into(),
        haproxy_spoa_hub_plugin_api::SpoeValue::String("10.0.0.2".into()),
    );
    args.insert(
        "method".into(),
        haproxy_spoa_hub_plugin_api::SpoeValue::String("GET".into()),
    );
    args.insert(
        "path".into(),
        haproxy_spoa_hub_plugin_api::SpoeValue::String("/api/v1/users".into()),
    );
    let msg = SpoeMessage {
        name: "bench-msg".into(),
        args,
        stream_id: 1,
        frame_id: 1,
    };
    let plan = registry.plan_for_message("bench-msg").unwrap().clone();

    c.bench_function("dispatch_1_plugin_4_args", |b| {
        b.iter(|| rt.block_on(dispatch_message(&msg, &plan, &registry)));
    });
}

criterion_group!(
    benches,
    bench_dispatch_single_plugin,
    bench_dispatch_two_plugins_parallel,
    bench_dispatch_with_args
);
criterion_main!(benches);
