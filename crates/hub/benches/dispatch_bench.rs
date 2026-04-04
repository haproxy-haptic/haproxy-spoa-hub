use std::path::PathBuf;
use std::sync::Arc;

use abi_stable::std_types::RHashMap;
use criterion::{Criterion, criterion_group, criterion_main};
use haproxy_spoa_hub::dispatch::dispatch_message;
use haproxy_spoa_hub::registry::PluginRegistry;
use haproxy_spoa_hub_plugin_api::{SpoeMessage, SpoeValue};

fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
}

fn noop_bench_config(name: &str, message: &str) -> haproxy_spoa_hub::config::PluginConfig {
    haproxy_spoa_hub::config::PluginConfig {
        name: name.into(),
        library: "libnoop_plugin.so".into(),
        messages: vec![message.into()],
        depends_on: vec![],
        timeout_ms: Some(5000),
        params: toml::Table::default(),
    }
}

fn build_single_noop_registry() -> Arc<PluginRegistry> {
    let dir = workspace_target_dir();
    let config = noop_bench_config("noop", "bench-msg");
    let loaded = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config, 5000).unwrap();
    Arc::new(PluginRegistry::build(vec![loaded]).unwrap())
}

fn build_two_plugin_registry() -> Arc<PluginRegistry> {
    let dir = workspace_target_dir();
    let a = haproxy_spoa_hub::plugin_loader::load_plugin(
        &dir,
        &noop_bench_config("noop-a", "bench-parallel"),
        5000,
    )
    .unwrap();
    let b = haproxy_spoa_hub::plugin_loader::load_plugin(
        &dir,
        &noop_bench_config("noop-b", "bench-parallel"),
        5000,
    )
    .unwrap();
    Arc::new(PluginRegistry::build(vec![a, b]).unwrap())
}

fn bench_dispatch_single_plugin(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = build_single_noop_registry();
    let msg = SpoeMessage {
        name: "bench-msg".into(),
        args: RHashMap::new(),
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
        args: RHashMap::new(),
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
    let mut args = RHashMap::new();
    args.insert("src".into(), SpoeValue::String("10.0.0.1".into()));
    args.insert("dst".into(), SpoeValue::String("10.0.0.2".into()));
    args.insert("method".into(), SpoeValue::String("GET".into()));
    args.insert("path".into(), SpoeValue::String("/api/v1/users".into()));
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
