use std::sync::Arc;

use abi_stable::std_types::RHashMap;
use criterion::{Criterion, criterion_group, criterion_main};
use haproxy_spoa_hub::dispatch::dispatch_message;
use haproxy_spoa_hub::registry::PluginRegistry;
use haproxy_spoa_hub::test_support::{load_test_plugins, plugin_config};
use haproxy_spoa_hub_plugin_api::{SpoeMessage, SpoeValue};

fn bench_dispatch_single_plugin(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let configs = [plugin_config(
        "noop",
        "libnoop_plugin.so",
        &["bench-msg"],
        5000,
    )];
    let registry = Arc::new(PluginRegistry::build(load_test_plugins(&configs, 5000)).unwrap());
    let msg = SpoeMessage {
        name: "bench-msg".into(),
        args: RHashMap::new(),
        stream_id: 1,
        frame_id: 1,
    };
    let plan = registry.plan_for_message("bench-msg").unwrap();

    c.bench_function("dispatch_1_plugin", |b| {
        b.iter(|| rt.block_on(dispatch_message(&msg, plan, &registry)));
    });
}

fn bench_dispatch_two_plugins_parallel(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let configs = [
        plugin_config("noop-a", "libnoop_plugin.so", &["bench-parallel"], 5000),
        plugin_config("noop-b", "libnoop_plugin.so", &["bench-parallel"], 5000),
    ];
    let registry = Arc::new(PluginRegistry::build(load_test_plugins(&configs, 5000)).unwrap());
    let msg = SpoeMessage {
        name: "bench-parallel".into(),
        args: RHashMap::new(),
        stream_id: 1,
        frame_id: 1,
    };
    let plan = registry.plan_for_message("bench-parallel").unwrap();

    c.bench_function("dispatch_2_plugins_parallel", |b| {
        b.iter(|| rt.block_on(dispatch_message(&msg, plan, &registry)));
    });
}

fn bench_dispatch_with_args(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let configs = [plugin_config(
        "noop",
        "libnoop_plugin.so",
        &["bench-msg"],
        5000,
    )];
    let registry = Arc::new(PluginRegistry::build(load_test_plugins(&configs, 5000)).unwrap());
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
    let plan = registry.plan_for_message("bench-msg").unwrap();

    c.bench_function("dispatch_1_plugin_4_args", |b| {
        b.iter(|| rt.block_on(dispatch_message(&msg, plan, &registry)));
    });
}

criterion_group!(
    benches,
    bench_dispatch_single_plugin,
    bench_dispatch_two_plugins_parallel,
    bench_dispatch_with_args
);
criterion_main!(benches);
